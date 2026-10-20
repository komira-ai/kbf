//! The beta smoke workflow (`.github/workflows/beta.yml`) fetches the same Buck2
//! as `ci.yml` and runs the same actions, and its text says so by copying `ci.yml`'s pins:
//! `BUCK2_VERSION`, `BUCK2_SHA256` and each action's commit.
//! A bump in `ci.yml` that leaves a copy behind would have the smoke test other bytes than
//! the rest of CI, with nothing red to say so. [`mismatches`] compares them: every pin the
//! smoke job sets must be set in some job of `ci.yml`, to the same value in every job that
//! sets it, and every action the smoke job uses must be used by `ci.yml` at the same commit.

use std::collections::BTreeMap;

use super::workflows;

/// The smoke workflow and the one it copies from.
const BETA: &str = ".github/workflows/beta.yml";
const CI: &str = ".github/workflows/ci.yml";

/// The job of the smoke workflow that holds the pins.
const JOB: &str = "smoke";

/// The environment variables that name a tool and its checksum.
const PINS: [&str; 2] = ["BUCK2_VERSION", "BUCK2_SHA256"];

/// `uses:` `action@commit` split at the `@`.
fn split(uses: &str) -> Option<(&str, &str)> {
    uses.split_once('@')
}

/// Every way the smoke workflow's pins differ from `ci`'s, one sentence each; an error when
/// either text does not parse.
fn mismatches(beta: &str, ci: &str) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let beta_env = workflows::job_env(beta, JOB)?;
    let mut ci_env: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut ci_uses: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for job in workflows::job_names(ci)? {
        for (key, value) in workflows::job_env(ci, &job)? {
            ci_env.entry(key).or_default().push((job.clone(), value));
        }
        for step in workflows::job_steps(ci, &job)? {
            if let Some((name, commit)) = step.uses.as_deref().and_then(split) {
                ci_uses
                    .entry(name.to_owned())
                    .or_default()
                    .push(commit.to_owned());
            }
        }
    }
    for pin in PINS {
        let Some((_, mine)) = beta_env.iter().find(|(k, _)| k == pin) else {
            problems.push(format!("{BETA}: the {JOB} job does not set {pin}"));
            continue;
        };
        match ci_env.get(pin) {
            None => problems.push(format!("{CI}: no job sets {pin}, which {BETA} copies")),
            Some(theirs) => {
                for (job, value) in theirs.iter().filter(|(_, v)| v != mine) {
                    problems.push(format!(
                        "{BETA}: {pin} is {mine}, but {CI} job {job} has {value}"
                    ));
                }
            }
        }
    }
    for step in workflows::job_steps(beta, JOB)? {
        let Some((name, commit)) = step.uses.as_deref().and_then(split) else {
            continue;
        };
        match ci_uses.get(name) {
            None => problems.push(format!("{BETA}: {name} is used, and {CI} does not use it")),
            Some(commits) if commits.iter().any(|c| c != commit) => problems.push(format!(
                "{BETA}: {name} is at {commit}, but {CI} uses it at {}",
                commits
                    .iter()
                    .filter(|c| *c != commit)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Some(_) => {}
        }
    }
    Ok(problems)
}

#[test]
fn the_beta_workflow_copies_the_pins_of_ci() {
    // Catches: a Buck2 or action bump in ci.yml that leaves beta.yml on the old pin.
    let root = super::repo_root();
    let read = |path: &str| std::fs::read_to_string(root.join(path)).expect(path);
    assert_eq!(mismatches(&read(BETA), &read(CI)), Ok(Vec::new()));
}

/// The rules [`mismatches`] reads the two workflows by, on planted ones.
mod rules {
    use super::*;

    const SHA_A: &str = "3d3c42e5aac5ba805825da76410c181273ba90b1";
    const SHA_B: &str = "55cc8345863c7cc4c66a329aec7e433d2d1c52a9";

    /// A workflow of one job `job` with `env` lines and `steps`.
    fn workflow(job: &str, env: &str, steps: &str) -> String {
        format!(
            "on: push\npermissions: {{}}\njobs:\n  {job}:\n    runs-on: ubuntu-24.04\n    env:\n{env}    steps:\n{steps}"
        )
    }

    fn env(version: &str) -> String {
        format!("      BUCK2_VERSION: {version}\n      BUCK2_SHA256: aa\n")
    }

    fn uses(name: &str, sha: &str) -> String {
        format!("      - uses: {name}@{sha} # v1\n")
    }

    #[test]
    fn equal_pins_pass_and_ci_may_set_them_in_more_than_one_job_if_all_agree() {
        // Catches: the rule refusing what CI is: BUCK2 pinned in several jobs, actions the
        // smoke job does not use.
        let beta = workflow("smoke", &env("1"), &uses("actions/checkout", SHA_A));
        let ci = format!(
            "{}{}",
            workflow(
                "test",
                &env("1"),
                &(uses("actions/checkout", SHA_A) + &uses("actions/x", SHA_B))
            ),
            "  other:\n    runs-on: ubuntu-24.04\n    env:\n      BUCK2_VERSION: 1\n    steps:\n      - run: true\n"
        );
        assert_eq!(mismatches(&beta, &ci), Ok(Vec::new()));
    }

    #[test]
    fn each_drift_is_named() {
        // Catches: each way the copy goes stale accepted: a version bumped on one side, a
        // checksum, an action's commit, a pin the smoke job lacks, a pin ci.yml no longer
        // sets, an action only the smoke job uses.
        let ci = workflow("test", &env("1"), &uses("actions/checkout", SHA_A));
        let beta = workflow("smoke", &env("2"), &uses("actions/checkout", SHA_A));
        let found = mismatches(&beta, &ci).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("BUCK2_VERSION is 2, but") && found[0].contains("has 1"),
            "{found:?}"
        );

        let beta = workflow("smoke", &env("1"), &uses("actions/checkout", SHA_B));
        let found = mismatches(&beta, &ci).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("actions/checkout is at") && found[0].contains(SHA_A),
            "{found:?}"
        );

        let beta = workflow(
            "smoke",
            "      BUCK2_VERSION: 1\n",
            &uses("actions/checkout", SHA_A),
        );
        let found = mismatches(&beta, &ci).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found.iter().all(|f| f.contains("does not set")),
            "{found:?}"
        );

        let bare = workflow("test", "      OTHER: 1\n", &uses("actions/checkout", SHA_A));
        let beta = workflow("smoke", &env("1"), &uses("actions/checkout", SHA_A));
        let found = mismatches(&beta, &bare).expect("both parse");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.contains("no job sets")), "{found:?}");

        let beta = workflow("smoke", &env("1"), &uses("actions/new", SHA_A));
        let found = mismatches(&beta, &ci).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("actions/new is used"), "{found:?}");
    }

    #[test]
    fn a_ci_job_that_disagrees_is_named_even_when_another_agrees() {
        // Catches: the first job that matches settling it (ci.yml pins Buck2 in two jobs,
        // and one is stale).
        let ci = format!(
            "{}{}",
            workflow("a", &env("1"), &uses("actions/checkout", SHA_A)),
            "  b:\n    runs-on: ubuntu-24.04\n    env:\n      BUCK2_VERSION: 0\n    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v1\n"
        );
        let beta = workflow("smoke", &env("1"), &uses("actions/checkout", SHA_A));
        let found = mismatches(&beta, &ci).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("job b has 0"), "{found:?}");
    }

    #[test]
    fn a_workflow_that_does_not_parse_or_lacks_the_job_is_an_error_not_a_pass() {
        // Catches: a lint that passes because it read nothing.
        assert!(mismatches("jobs: [", "on: push").is_err());
        let ci = workflow("test", &env("1"), "");
        assert!(mismatches(&ci, &ci).is_err(), "no smoke job");
    }
}
