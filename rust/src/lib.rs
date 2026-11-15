//! Chef/CINC cookbook dependency solving on top of the `pubgrub` crate.
//!
//! The problem format (JSON) mirrors what a Chef server knows at solve time:
//!
//! ```json
//! {
//!   "cookbooks":   { "app": { "1.1.0": { "lib": "~> 1.0" }, "1.0.0": {} } },
//!   "run_list":    { "app": "= 1.1.0" },
//!   "environment": { "lib": "< 2.0.0" }
//! }
//! ```
//!
//! * `cookbooks` is the universe: every cookbook version carries its own dependencies.
//! * `run_list` is what must be installed, with a constraint per cookbook.
//! * `environment` (optional) restricts which versions are eligible at all, without
//!   forcing the cookbook into the solution.
//! * `timeout_ms` (optional) bounds the solve; without it the solver runs to completion.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::c_char;
use std::time::{Duration, Instant};

use pubgrub::{
    DefaultStringReporter, Dependencies, DependencyProvider, DerivationTree, External,
    OfflineDependencyProvider, PackageResolutionStatistics, PubGrubError, Ranges, Reporter, resolve,
};
use serde::{Deserialize, Serialize};

/// A Chef cookbook version: `x.y` or `x.y.z`, numeric only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self { major, minor, patch }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        Self::parse_with_precision(raw).map(|(version, _)| version)
    }

    /// Parses a version and reports how many components were written (2 or 3),
    /// which `~>` needs to know.
    fn parse_with_precision(raw: &str) -> Result<(Self, usize), String> {
        let text = raw.trim();
        let pieces: Vec<&str> = text.split('.').collect();
        if pieces.len() < 2 || pieces.len() > 3 {
            return Err(format!("invalid cookbook version {raw:?}: expected x.y or x.y.z"));
        }
        let mut numbers = [0u64; 3];
        for (slot, piece) in numbers.iter_mut().zip(&pieces) {
            if piece.is_empty() || !piece.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(format!("invalid cookbook version {raw:?}: expected x.y or x.y.z"));
            }
            *slot = piece
                .parse()
                .map_err(|_| format!("invalid cookbook version {raw:?}: component out of range"))?;
        }
        Ok((Self::new(numbers[0], numbers[1], numbers[2]), pieces.len()))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A parsed Chef version constraint: operator, version, and how many version
/// components were written (which `~>` depends on). `None` means "any version".
type ParsedConstraint = Option<(&'static str, Version, usize)>;

fn parse_constraint_parts(raw: &str) -> Result<ParsedConstraint, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Ok(None);
    }
    // Two-character operators first so ">=" is not read as ">".
    const OPERATORS: [&str; 6] = ["~>", ">=", "<=", "=", ">", "<"];
    let (operator, rest) = OPERATORS
        .iter()
        .find_map(|op| text.strip_prefix(op).map(|rest| (*op, rest)))
        .unwrap_or(("=", text));
    let (version, precision) =
        Version::parse_with_precision(rest).map_err(|err| format!("invalid constraint {raw:?}: {err}"))?;
    Ok(Some((operator, version, precision)))
}

/// Parses a Chef version constraint (`= 1.0.0`, `>= 1.0`, `~> 1.2`, bare `1.2.3`, ...)
/// into the set of versions it allows. An empty constraint allows everything.
pub fn parse_constraint(raw: &str) -> Result<Ranges<Version>, String> {
    let Some((operator, version, precision)) = parse_constraint_parts(raw)? else {
        return Ok(Ranges::full());
    };
    Ok(match operator {
        "=" => Ranges::singleton(version),
        ">" => Ranges::strictly_higher_than(version),
        ">=" => Ranges::higher_than(version),
        "<" => Ranges::strictly_lower_than(version),
        "<=" => Ranges::lower_than(version),
        // "~> 1.2" allows < 2.0.0; "~> 1.2.3" allows < 1.3.0.
        _ => {
            let upper = if precision == 2 {
                Version::new(version.major + 1, 0, 0)
            } else {
                Version::new(version.major, version.minor + 1, 0)
            };
            Ranges::between(version, upper)
        }
    })
}

/// Renders a run-list requirement the way Chef Server reports it, e.g. `(foo = 3.0.0)`
/// or `(foo >= 0.0.0)` for an unconstrained item.
fn describe_requirement(name: &str, raw: &str) -> String {
    match parse_constraint_parts(raw) {
        Ok(Some(("~>", version, 2))) => format!("({name} ~> {}.{})", version.major, version.minor),
        Ok(Some((operator, version, _))) => format!("({name} {operator} {version})"),
        _ => format!("({name} >= 0.0.0)"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Package {
    /// Synthetic package whose dependencies are the run list.
    Root,
    Cookbook(String),
}

impl fmt::Display for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Package::Root => write!(f, "the run list"),
            Package::Cookbook(name) => write!(f, "{name}"),
        }
    }
}

/// name -> version -> (dependency name -> constraint)
pub type Universe = BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub cookbooks: Universe,
    pub run_list: BTreeMap<String, String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Give up after this many milliseconds. No limit when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    /// A consistent set of cookbook versions.
    Ok { solution: BTreeMap<String, String> },
    /// No solution exists; `explanation` is the human-readable derivation.
    /// `non_existent_cookbooks` lists cookbooks the conflict involves that are not in
    /// the universe at all.
    Unsolvable { explanation: String, non_existent_cookbooks: Vec<String> },
    /// The run list cannot be satisfied on its own terms, before any dependencies are
    /// considered: it names cookbooks that do not exist, or asks for versions that do
    /// not exist (or that the environment excludes). `no_versions` holds the offending
    /// requirements, e.g. `(foo = 3.0.0)`.
    InvalidRunList { non_existent_cookbooks: Vec<String>, no_versions: Vec<String> },
    /// `timeout_ms` elapsed before the solver finished.
    Timeout { message: String },
    /// The problem itself was malformed.
    Error { message: String },
}

pub fn solve(problem: &Problem) -> Outcome {
    match solve_inner(problem) {
        Ok(outcome) => outcome,
        Err(message) => Outcome::Error { message },
    }
}

#[derive(Debug)]
struct DeadlineExceeded;

impl fmt::Display for DeadlineExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the time limit was reached")
    }
}

impl std::error::Error for DeadlineExceeded {}

type Offline = OfflineDependencyProvider<Package, Ranges<Version>>;

/// The in-memory universe, plus an optional deadline that cancels the solve.
struct Provider {
    universe: Offline,
    deadline: Option<Instant>,
}

impl DependencyProvider for Provider {
    type P = Package;
    type V = Version;
    type VS = Ranges<Version>;
    type M = String;
    type Priority = <Offline as DependencyProvider>::Priority;
    type Err = DeadlineExceeded;

    fn prioritize(
        &self,
        package: &Package,
        range: &Ranges<Version>,
        statistics: &PackageResolutionStatistics,
    ) -> Self::Priority {
        self.universe.prioritize(package, range, statistics)
    }

    /// Newest version in range, as OfflineDependencyProvider does.
    fn choose_version(&self, package: &Package, range: &Ranges<Version>) -> Result<Option<Version>, DeadlineExceeded> {
        match self.universe.choose_version(package, range) {
            Ok(version) => Ok(version),
            Err(never) => match never {},
        }
    }

    fn get_dependencies(
        &self,
        package: &Package,
        version: &Version,
    ) -> Result<Dependencies<Package, Ranges<Version>, String>, DeadlineExceeded> {
        match self.universe.get_dependencies(package, version) {
            Ok(dependencies) => Ok(dependencies),
            Err(never) => match never {},
        }
    }

    fn should_cancel(&self) -> Result<(), DeadlineExceeded> {
        match self.deadline {
            Some(deadline) if Instant::now() >= deadline => Err(DeadlineExceeded),
            _ => Ok(()),
        }
    }
}

/// Cookbooks a failed derivation needed that have no versions in the universe at all.
fn non_existent_cookbooks(
    tree: &DerivationTree<Package, Ranges<Version>, String>,
    known: &BTreeSet<&str>,
    found: &mut BTreeSet<String>,
) {
    match tree {
        DerivationTree::External(External::NoVersions(Package::Cookbook(name), _)) => {
            if !known.contains(name.as_str()) {
                found.insert(name.clone());
            }
        }
        DerivationTree::External(_) => {}
        DerivationTree::Derived(derived) => {
            non_existent_cookbooks(&derived.cause1, known, found);
            non_existent_cookbooks(&derived.cause2, known, found);
        }
    }
}

fn solve_inner(problem: &Problem) -> Result<Outcome, String> {
    let deadline = problem.timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));

    let environment = problem
        .environment
        .iter()
        .map(|(name, constraint)| {
            parse_constraint(constraint)
                .map(|ranges| (name.as_str(), ranges))
                .map_err(|err| format!("environment constraint on {name}: {err}"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    let mut universe = Offline::new();
    // Versions the environment allows, per cookbook; used to validate the run list.
    let mut eligible: BTreeMap<&str, Vec<Version>> = BTreeMap::new();

    for (name, versions) in &problem.cookbooks {
        let allowed = eligible.entry(name.as_str()).or_default();
        for (raw_version, dependencies) in versions {
            let version = Version::parse(raw_version).map_err(|err| format!("cookbook {name}: {err}"))?;
            if environment.get(name.as_str()).is_some_and(|allowed| !allowed.contains(&version)) {
                continue;
            }
            let parsed = dependencies
                .iter()
                .map(|(dependency, constraint)| {
                    parse_constraint(constraint)
                        .map(|ranges| (Package::Cookbook(dependency.clone()), ranges))
                        .map_err(|err| format!("{name} {version} depends on {dependency}: {err}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            universe.add_dependencies(Package::Cookbook(name.clone()), version, parsed);
            allowed.push(version);
        }
    }

    let root_version = Version::new(0, 0, 0);
    let mut run_list = Vec::new();
    let mut missing = Vec::new();
    let mut no_versions = Vec::new();
    for (name, constraint) in &problem.run_list {
        let ranges = parse_constraint(constraint).map_err(|err| format!("run list item {name}: {err}"))?;
        match eligible.get(name.as_str()) {
            None => missing.push(name.clone()),
            Some(versions) if !versions.iter().any(|version| ranges.contains(version)) => {
                no_versions.push(describe_requirement(name, constraint));
            }
            Some(_) => {}
        }
        run_list.push((Package::Cookbook(name.clone()), ranges));
    }
    if !missing.is_empty() || !no_versions.is_empty() {
        return Ok(Outcome::InvalidRunList { non_existent_cookbooks: missing, no_versions });
    }
    universe.add_dependencies(Package::Root, root_version, run_list);

    let provider = Provider { universe, deadline };
    match resolve(&provider, Package::Root, root_version) {
        Ok(selected) => Ok(Outcome::Ok {
            solution: selected
                .into_iter()
                .filter_map(|(package, version)| match package {
                    Package::Root => None,
                    Package::Cookbook(name) => Some((name, version.to_string())),
                })
                .collect(),
        }),
        Err(PubGrubError::NoSolution(mut tree)) => {
            let known: BTreeSet<&str> = problem.cookbooks.keys().map(String::as_str).collect();
            let mut absent = BTreeSet::new();
            non_existent_cookbooks(&tree, &known, &mut absent);
            tree.collapse_no_versions();
            Ok(Outcome::Unsolvable {
                explanation: DefaultStringReporter::report(&tree),
                non_existent_cookbooks: absent.into_iter().collect(),
            })
        }
        Err(PubGrubError::ErrorInShouldCancel(_)) => Ok(Outcome::Timeout {
            message: format!("no answer within {}ms", problem.timeout_ms.unwrap_or_default()),
        }),
        Err(other) => Err(other.to_string()),
    }
}

/// Solves a JSON-encoded [`Problem`] and returns a JSON-encoded [`Outcome`].
pub fn solve_json(input: &str) -> String {
    let outcome = match serde_json::from_str::<Problem>(input) {
        Ok(problem) => solve(&problem),
        Err(err) => Outcome::Error { message: format!("invalid problem JSON: {err}") },
    };
    serde_json::to_string(&outcome).expect("Outcome always serializes")
}

/// C ABI: takes a NUL-terminated JSON [`Problem`], returns a NUL-terminated JSON
/// [`Outcome`] that must be released with [`depsolve_free`]. Returns null only if
/// `input` is null.
///
/// # Safety
/// `input` must be null or point to a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn depsolve_solve(input: *const c_char) -> *mut c_char {
    if input.is_null() {
        return std::ptr::null_mut();
    }
    let text = unsafe { CStr::from_ptr(input) }.to_string_lossy();
    let output = std::panic::catch_unwind(|| solve_json(&text)).unwrap_or_else(|_| {
        r#"{"status":"error","message":"internal solver panic"}"#.to_string()
    });
    CString::new(output).expect("JSON output has no interior NUL").into_raw()
}

/// # Safety
/// `ptr` must be null or a pointer previously returned by [`depsolve_solve`].
#[no_mangle]
pub unsafe extern "C" fn depsolve_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(raw: &str) -> Version {
        Version::parse(raw).unwrap()
    }

    fn problem(json: &str) -> Problem {
        serde_json::from_str(json).unwrap()
    }

    fn solution(json: &str) -> BTreeMap<String, String> {
        match solve(&problem(json)) {
            Outcome::Ok { solution } => solution,
            other => panic!("expected a solution, got {other:?}"),
        }
    }

    #[test]
    fn versions_parse_and_order_numerically() {
        assert_eq!(v("1.2"), v("1.2.0"));
        assert!(v("1.10.0") > v("1.9.0"));
        for bad in ["1", "1.2.3.4", "1.x", "", "1..2", "v1.2.3"] {
            assert!(Version::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn comparison_operators() {
        let cases = [
            ("= 1.2.3", "1.2.3", true),
            ("1.2.3", "1.2.4", false),
            ("> 1.2.3", "1.2.3", false),
            ("> 1.2.3", "1.2.4", true),
            (">= 1.2.3", "1.2.3", true),
            ("< 2.0", "1.9.9", true),
            ("< 2.0", "2.0.0", false),
            ("<= 2.0", "2.0.0", true),
            (">=1.0.0", "0.9.0", false),
            ("", "0.0.1", true),
        ];
        for (constraint, version, expected) in cases {
            assert_eq!(
                parse_constraint(constraint).unwrap().contains(&v(version)),
                expected,
                "{constraint:?} vs {version}"
            );
        }
    }

    #[test]
    fn pessimistic_operator_follows_chef_semantics() {
        let two_part = parse_constraint("~> 1.2").unwrap();
        assert!(!two_part.contains(&v("1.1.9")));
        assert!(two_part.contains(&v("1.2.0")));
        assert!(two_part.contains(&v("1.9.9")));
        assert!(!two_part.contains(&v("2.0.0")));

        let three_part = parse_constraint("~> 1.2.3").unwrap();
        assert!(!three_part.contains(&v("1.2.2")));
        assert!(three_part.contains(&v("1.2.3")));
        assert!(three_part.contains(&v("1.2.99")));
        assert!(!three_part.contains(&v("1.3.0")));
    }

    #[test]
    fn malformed_constraints_are_rejected() {
        for bad in ["!= 1.0.0", ">= 1.0.0, < 2.0.0", "~> 1", "latest"] {
            assert!(parse_constraint(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn picks_newest_versions_that_satisfy_constraints() {
        let found = solution(
            r#"{
                "cookbooks": {
                    "app": {"1.0.0": {}, "1.1.0": {"lib": "~> 1.0"}},
                    "lib": {"1.0.0": {}, "1.5.0": {}, "2.0.0": {}}
                },
                "run_list": {"app": "= 1.1.0"}
            }"#,
        );
        assert_eq!(found, BTreeMap::from([("app".into(), "1.1.0".into()), ("lib".into(), "1.5.0".into())]));
    }

    #[test]
    fn dependencies_are_per_version() {
        // app 2.0.0 needs a lib that does not exist, so the solver must fall back to 1.0.0.
        let found = solution(
            r#"{
                "cookbooks": {
                    "app": {"1.0.0": {"lib": "~> 1.0"}, "2.0.0": {"lib": "~> 3.0"}},
                    "lib": {"1.4.0": {}, "2.0.0": {}}
                },
                "run_list": {"app": ">= 0.0.0"}
            }"#,
        );
        assert_eq!(found, BTreeMap::from([("app".into(), "1.0.0".into()), ("lib".into(), "1.4.0".into())]));
    }

    #[test]
    fn late_pin_forces_an_early_choice_back() {
        let found = solution(
            r#"{
                "cookbooks": {
                    "pk0": {"1.1.0": {"pk1": "~> 1.0"}, "1.2.0": {"pk1": "~> 1.0"}, "1.3.0": {"pk1": "~> 1.0"}},
                    "pk1": {"1.1.0": {"late": "~> 1.0"}, "1.2.0": {"late": "~> 1.0"}},
                    "late": {"1.1.0": {"pk0": "= 1.1.0"}, "1.2.0": {"pk0": "= 1.1.0"}}
                },
                "run_list": {"pk0": "~> 1.0"}
            }"#,
        );
        assert_eq!(found["pk0"], "1.1.0");
        assert_eq!(found["pk1"], "1.2.0");
        assert_eq!(found["late"], "1.2.0");
    }

    #[test]
    fn environment_restricts_without_requiring() {
        let json = r#"{
            "cookbooks": {
                "app": {"1.0.0": {"lib": ">= 1.0.0"}},
                "lib": {"1.0.0": {}, "2.0.0": {}},
                "unused": {"1.0.0": {}}
            },
            "run_list": {"app": "= 1.0.0"},
            "environment": {"lib": "< 2.0.0", "unused": "= 1.0.0"}
        }"#;
        let found = solution(json);
        assert_eq!(found, BTreeMap::from([("app".into(), "1.0.0".into()), ("lib".into(), "1.0.0".into())]));
    }

    #[test]
    fn conflicts_are_explained() {
        let outcome = solve(&problem(
            r#"{
                "cookbooks": {
                    "app": {"1.0.0": {"lib": "~> 1.0"}},
                    "lib": {"1.0.0": {}, "2.0.0": {}}
                },
                "run_list": {"app": "= 1.0.0", "lib": "= 2.0.0"}
            }"#,
        ));
        let Outcome::Unsolvable { explanation, non_existent_cookbooks } = outcome else {
            panic!("expected unsolvable, got {outcome:?}");
        };
        assert!(explanation.contains("app") && explanation.contains("lib"), "{explanation}");
        assert!(non_existent_cookbooks.is_empty());
    }

    #[test]
    fn missing_cookbook_is_unsolvable_not_an_error() {
        let outcome = solve(&problem(
            r#"{"cookbooks": {"app": {"1.0.0": {"ghost": ">= 0.0.0"}}}, "run_list": {"app": "= 1.0.0"}}"#,
        ));
        let Outcome::Unsolvable { non_existent_cookbooks, .. } = outcome else {
            panic!("expected unsolvable, got {outcome:?}");
        };
        assert_eq!(non_existent_cookbooks, ["ghost"]);
    }

    #[test]
    fn run_list_is_validated_before_solving() {
        let outcome = solve(&problem(
            r#"{
                "cookbooks": {"foo": {"1.2.3": {}}, "bar": {"2.0.0": {}}, "baz": {"1.0.0": {}}, "qux": {"1.0": {}}},
                "run_list": {"foo": "= 3.0.0", "bar": "= 2.0.0", "baz": "", "nope": "", "qux": "~> 2.0"},
                "environment": {"baz": "> 1.0.0"}
            }"#,
        ));
        assert_eq!(
            outcome,
            Outcome::InvalidRunList {
                non_existent_cookbooks: vec!["nope".into()],
                no_versions: vec!["(baz >= 0.0.0)".into(), "(foo = 3.0.0)".into(), "(qux ~> 2.0)".into()],
            }
        );
    }

    #[test]
    fn timeout_stops_the_solve() {
        let json = r#"{"cookbooks": {"app": {"1.0.0": {}}}, "run_list": {"app": ""}, "timeout_ms": 0}"#;
        assert!(matches!(solve(&problem(json)), Outcome::Timeout { .. }));

        let generous = json.replace(r#""timeout_ms": 0"#, r#""timeout_ms": 60000"#);
        assert!(matches!(solve(&problem(&generous)), Outcome::Ok { .. }));
    }

    #[test]
    fn malformed_input_reports_an_error() {
        let outcome = solve(&problem(
            r#"{"cookbooks": {"app": {"1.0.0": {"lib": "!= 1.0.0"}}}, "run_list": {"app": "= 1.0.0"}}"#,
        ));
        let Outcome::Error { message } = outcome else {
            panic!("expected an error, got {outcome:?}");
        };
        assert!(message.contains("app 1.0.0 depends on lib"), "{message}");

        assert!(solve_json("not json").contains(r#""status":"error""#));
    }

    #[test]
    fn c_abi_round_trip() {
        let input = CString::new(r#"{"cookbooks": {"app": {"1.0.0": {}}}, "run_list": {"app": ""}}"#).unwrap();
        unsafe {
            let raw = depsolve_solve(input.as_ptr());
            let text = CStr::from_ptr(raw).to_str().unwrap().to_owned();
            depsolve_free(raw);
            assert_eq!(text, r#"{"status":"ok","solution":{"app":"1.0.0"}}"#);
            assert!(depsolve_solve(std::ptr::null()).is_null());
        }
    }
}
