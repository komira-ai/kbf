//! The beta smoke job (`smoke` in `.github/workflows/smoke.yml`) fetches the same Buck2
//! as the jobs of `beta.yml` (the M1 and M2 cells) and runs the same actions, and
//! its text says so by copying their pins: `BUCK2_VERSION`, `BUCK2_SHA256` and each
//! action's commit.
//! A bump in one job that leaves a copy behind would have the smoke test other bytes than
//! the rest of CI, with nothing red to say so. [`mismatches`] compares them: every pin the
//! smoke job sets must be set in some other job, to the same value in every job that sets
//! it, and every action the smoke job uses must be used by another job at the same commit.

use std::collections::BTreeMap;

use super::workflows;

/// The workflow that holds the smoke job.
const SMOKE: &str = ".github/workflows/smoke.yml";
/// The workflow that holds the jobs the smoke job copies from, and the cross-version cell.
const BETA: &str = ".github/workflows/beta.yml";
/// What a message calls the jobs the smoke job copies from.
const CI: &str = ".github/workflows/beta.yml (another job)";

/// The job of the smoke workflow that holds the pins.
const JOB: &str = "smoke";

/// The environment variables that name a tool and its checksum.
const PINS: [&str; 2] = ["BUCK2_VERSION", "BUCK2_SHA256"];

/// `uses:` `action@commit` split at the `@`.
fn split(uses: &str) -> Option<(&str, &str)> {
    uses.split_once('@')
}

/// Every way the smoke workflow's pins differ from `ci`'s, one sentence each; an error when
/// either text does not parse. The actions the other jobs of `also` (workflows that use the
/// same actions, such as the artifacts workflow whose files the smoke downloads) use count as
/// uses by another job.
fn mismatches(beta: &str, ci: &str, also: &[&str]) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let beta_env = workflows::job_env(beta, JOB)?;
    let mut ci_env: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut ci_uses: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for job in workflows::job_names(ci)?.into_iter().filter(|j| j != JOB) {
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
    for text in also {
        for job in workflows::job_names(text)? {
            for step in workflows::job_steps(text, &job).unwrap_or_default() {
                if let Some((name, commit)) = step.uses.as_deref().and_then(split) {
                    ci_uses
                        .entry(name.to_owned())
                        .or_default()
                        .push(commit.to_owned());
                }
            }
        }
    }
    for pin in PINS {
        let Some((_, mine)) = beta_env.iter().find(|(k, _)| k == pin) else {
            problems.push(format!("{SMOKE}: the {JOB} job does not set {pin}"));
            continue;
        };
        match ci_env.get(pin) {
            None => problems.push(format!("{CI}: no job sets {pin}, which {SMOKE} copies")),
            Some(theirs) => {
                for (job, value) in theirs.iter().filter(|(_, v)| v != mine) {
                    problems.push(format!(
                        "{SMOKE}: {pin} is {mine}, but {CI} job {job} has {value}"
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
            None => problems.push(format!("{SMOKE}: {name} is used, and {CI} does not use it")),
            Some(commits) if commits.iter().any(|c| c != commit) => problems.push(format!(
                "{SMOKE}: {name} is at {commit}, but {CI} uses it at {}",
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

/// The workflow and job that fetch the same Buck2 to prove the farm after a deploy.
const RELEASE: &str = ".github/workflows/release.yml";
const GAMMA: &str = "gamma";

/// Every way the pins of the release workflow's gamma job differ from the smoke job's: the
/// proof must build with the Buck2 the beta run was tested with. An error when either text
/// does not parse or lacks its job.
fn gamma_mismatches(release: &str, beta: &str) -> Result<Vec<String>, String> {
    let theirs = workflows::job_env(beta, JOB)?;
    let mine = workflows::job_env(release, GAMMA)?;
    let mut problems = Vec::new();
    for pin in PINS {
        let want = theirs.iter().find(|(k, _)| k == pin).map(|(_, v)| v);
        let have = mine.iter().find(|(k, _)| k == pin).map(|(_, v)| v);
        match (want, have) {
            (Some(w), Some(h)) if w == h => {}
            (Some(w), Some(h)) => problems.push(format!(
                "{RELEASE}: {pin} is {h} in the {GAMMA} job, but {SMOKE} job {JOB} has {w}"
            )),
            (_, None) => problems.push(format!("{RELEASE}: the {GAMMA} job does not set {pin}")),
            (None, Some(_)) => problems.push(format!("{SMOKE}: the {JOB} job does not set {pin}")),
        }
    }
    Ok(problems)
}

/// Every step of `text` that fetches Buck2 (its `run:` names `$BUCK2_VERSION`) and has an
/// `env:` of its own: the pins come from the job's `env:`, which [`mismatches`] compares, and
/// a step's own would fetch other bytes. An error when the text does not parse.
fn fetch_step_overrides(text: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    for job in workflows::job_names(text)? {
        for step in workflows::job_steps(text, &job)? {
            let fetches = step
                .run
                .as_deref()
                .is_some_and(|r| r.contains("$BUCK2_VERSION"));
            if fetches && !step.env.is_empty() {
                found.push(format!(
                    "{BETA}: the Buck2 fetch step of job {job} has an `env:`; the pins are the job's"
                ));
            }
        }
    }
    Ok(found)
}

#[test]
fn no_buck2_fetch_step_overrides_the_pins_with_an_env_of_its_own() {
    // Catches: a step-level `env:` on a Buck2 fetch step in beta.yml or release.yml that
    // changes the checksum or the version after the job-level pins were compared.
    let root = super::repo_root();
    for path in [BETA, SMOKE, RELEASE] {
        let text = std::fs::read_to_string(root.join(path)).expect(path);
        assert_eq!(fetch_step_overrides(&text), Ok(Vec::new()), "{path}");
    }
    let planted = "on: push\npermissions: {}\njobs:\n  smoke:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo xtask fetch --url x/$BUCK2_VERSION --sha256 \"$BUCK2_SHA256\"\n        env:\n          BUCK2_SHA256: other\n";
    let found = fetch_step_overrides(planted).expect("parses");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(fetch_step_overrides("jobs: [").is_err());
}

#[test]
fn the_gamma_job_fetches_the_buck2_of_the_beta_smoke() {
    // Catches: a Buck2 bump in beta.yml that leaves the release's proof on the old bytes,
    // or the reverse.
    let root = super::repo_root();
    let read = |path: &str| std::fs::read_to_string(root.join(path)).expect(path);
    assert_eq!(
        gamma_mismatches(&read(RELEASE), &read(SMOKE)),
        Ok(Vec::new())
    );
}

#[test]
fn a_gamma_pin_that_differs_or_is_missing_is_named() {
    // Catches: the comparison passing when the version or checksum drifts, a pin is lost
    // from either side, or a workflow does not parse.
    let job = |name: &str, env: &str| {
        format!(
            "on: push\npermissions: {{}}\njobs:\n  {name}:\n    runs-on: ubuntu-24.04\n    env:\n{env}    steps:\n      - run: true\n"
        )
    };
    let pins = |v: &str, s: &str| format!("      BUCK2_VERSION: {v}\n      BUCK2_SHA256: {s}\n");
    let beta = job(JOB, &pins("1", "aa"));
    assert_eq!(
        gamma_mismatches(&job(GAMMA, &pins("1", "aa")), &beta),
        Ok(Vec::new())
    );
    let found = gamma_mismatches(&job(GAMMA, &pins("2", "bb")), &beta).expect("parses");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found[0].contains("BUCK2_VERSION is 2") && found[0].contains("has 1"),
        "{found:?}"
    );
    let found = gamma_mismatches(&job(GAMMA, "      OTHER: 1\n"), &beta).expect("parses");
    assert!(
        found.iter().all(|f| f.contains("does not set")),
        "{found:?}"
    );
    assert_eq!(found.len(), 2, "{found:?}");
    let found = gamma_mismatches(&job(GAMMA, &pins("1", "aa")), &job(JOB, "      OTHER: 1\n"))
        .expect("parses");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(gamma_mismatches("jobs: [", &beta).is_err());
    assert!(
        gamma_mismatches(&job("other", ""), &beta).is_err(),
        "no gamma job"
    );
}

#[test]
fn the_beta_workflow_copies_the_pins_of_ci() {
    // Catches: a Buck2 or action bump in another job of beta.yml that leaves the smoke job on
    // the old pin.
    let root = super::repo_root();
    let read = |path: &str| std::fs::read_to_string(root.join(path)).expect(path);
    let artifacts = read(".github/workflows/artifacts.yml");
    assert_eq!(
        mismatches(&read(SMOKE), &read(BETA), &[&artifacts]),
        Ok(Vec::new())
    );
}

/// The job of the cross-version cell, which builds release N from a pinned commit.
const CROSS_VERSION: &str = "cross-version";

/// Why the commit the cross-version job builds release N from is not a pin, or `None` when
/// it is one: `OLD_COMMIT` must be set in the job, as the 40 lower-case hex digits of a
/// commit (a branch or a tag would move under the job, and a short name could be ambiguous
/// or, once it is, fetch other bytes; a pin that is not a commit fails only when the
/// workflow runs on a push to main). An error when the workflow does not parse or has no
/// such job.
fn old_commit_problem(beta: &str) -> Result<Option<String>, String> {
    let env = workflows::job_env(beta, CROSS_VERSION)?;
    let Some((_, commit)) = env.iter().find(|(k, _)| k == "OLD_COMMIT") else {
        return Ok(Some(format!(
            "{BETA}: the {CROSS_VERSION} job does not set OLD_COMMIT"
        )));
    };
    let pinned = commit.len() == 40
        && commit
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    Ok((!pinned).then(|| {
        format!("{BETA}: OLD_COMMIT is {commit:?}, not the 40 lower-case hex digits of a commit")
    }))
}

#[test]
fn the_cross_version_job_builds_release_n_from_a_full_commit() {
    // Catches: the pin of release N written as a branch, a tag or an abbreviation, or lost.
    let root = super::repo_root();
    let beta = std::fs::read_to_string(root.join(BETA)).expect(BETA);
    assert_eq!(old_commit_problem(&beta), Ok(None));
}

/// The rule [`old_commit_problem`] reads the pin by, on planted workflows.
mod pin_rules {
    use super::*;

    fn workflow(env: &str) -> String {
        format!(
            "on: push\npermissions: {{}}\njobs:\n  cross-version:\n    runs-on: ubuntu-24.04\n    env:\n{env}    steps:\n      - run: true\n"
        )
    }

    #[test]
    fn only_a_full_lower_case_commit_is_a_pin() {
        // Catches: a pin accepted that is a branch, a tag, an abbreviation, upper case, one
        // digit short or long, or not hex; and the full commit refused.
        let full = "dfe8ceef77f9d63b9378cf2862de7e759624c0e9";
        let pinned = workflow(&format!("      OLD_COMMIT: {full}\n"));
        assert_eq!(old_commit_problem(&pinned), Ok(None));
        for bad in [
            "main",
            "v0.1.0",
            "dfe8ceef77f9",
            "DFE8CEEF77F9D63B9378CF2862DE7E759624C0E9",
            &full[..39],
            &format!("{full}0"),
            "gfe8ceef77f9d63b9378cf2862de7e759624c0e9",
        ] {
            let found = old_commit_problem(&workflow(&format!("      OLD_COMMIT: {bad}\n")));
            let found = found.expect("parses").expect("not a pin");
            assert!(found.contains("OLD_COMMIT is"), "{bad}: {found}");
        }
    }

    #[test]
    fn a_missing_pin_or_job_is_named_not_passed() {
        // Catches: a lint that passes because it read nothing.
        let found = old_commit_problem(&workflow("      OTHER: 1\n")).expect("parses");
        assert!(
            found
                .expect("a problem")
                .contains("does not set OLD_COMMIT")
        );
        assert!(old_commit_problem("jobs: [").is_err());
        let no_job = "on: push\npermissions: {}\njobs:\n  smoke:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n";
        assert!(old_commit_problem(no_job).is_err(), "no such job");
    }
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
        assert_eq!(mismatches(&beta, &ci, &[]), Ok(Vec::new()));
    }

    #[test]
    fn each_drift_is_named() {
        // Catches: each way the copy goes stale accepted: a version bumped on one side, a
        // checksum, an action's commit, a pin the smoke job lacks, a pin ci.yml no longer
        // sets, an action only the smoke job uses.
        let ci = workflow("test", &env("1"), &uses("actions/checkout", SHA_A));
        let beta = workflow("smoke", &env("2"), &uses("actions/checkout", SHA_A));
        let found = mismatches(&beta, &ci, &[]).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("BUCK2_VERSION is 2, but") && found[0].contains("has 1"),
            "{found:?}"
        );

        let beta = workflow("smoke", &env("1"), &uses("actions/checkout", SHA_B));
        let found = mismatches(&beta, &ci, &[]).expect("both parse");
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
        let found = mismatches(&beta, &ci, &[]).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found.iter().all(|f| f.contains("does not set")),
            "{found:?}"
        );

        let bare = workflow("test", "      OTHER: 1\n", &uses("actions/checkout", SHA_A));
        let beta = workflow("smoke", &env("1"), &uses("actions/checkout", SHA_A));
        let found = mismatches(&beta, &bare, &[]).expect("both parse");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.contains("no job sets")), "{found:?}");

        let beta = workflow("smoke", &env("1"), &uses("actions/new", SHA_A));
        let found = mismatches(&beta, &ci, &[]).expect("both parse");
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
        let found = mismatches(&beta, &ci, &[]).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("job b has 0"), "{found:?}");
    }

    #[test]
    fn a_workflow_that_does_not_parse_or_lacks_the_job_is_an_error_not_a_pass() {
        // Catches: a lint that passes because it read nothing.
        assert!(mismatches("jobs: [", "on: push", &[]).is_err());
        let ci = workflow("test", &env("1"), "");
        assert!(mismatches(&ci, &ci, &[]).is_err(), "no smoke job");
    }
}
