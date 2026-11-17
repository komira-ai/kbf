//! The smoke job (`smoke` in `.github/workflows/smoke.yml`) uses the same actions as the jobs of
//! `beta.yml` and `artifacts.yml`, and its text says so by copying their commits.
//! A bump in one job that leaves a copy behind would have the smoke job run other bytes than
//! the rest of CI, with nothing red to say so. [`mismatches`] compares them: every action the
//! smoke job uses must be used by another job at the same commit.

use std::collections::BTreeMap;

use super::workflows;

/// The workflow that holds the smoke job.
const SMOKE: &str = ".github/workflows/smoke.yml";
/// The workflow that holds the jobs the smoke job copies from.
const BETA: &str = ".github/workflows/beta.yml";
/// What a message calls the jobs the smoke job copies from.
const CI: &str = ".github/workflows/beta.yml (another job)";

/// The job of the smoke workflow that holds the actions.
const JOB: &str = "smoke";

/// `uses:` `action@commit` split at the `@`.
fn split(uses: &str) -> Option<(&str, &str)> {
    uses.split_once('@')
}

/// Every way the smoke workflow's actions differ from `ci`'s, one sentence each; an error when
/// either text does not parse. The actions the other jobs of `also` (workflows that use the
/// same actions, such as the artifacts workflow whose files the smoke downloads) use count as
/// uses by another job.
fn mismatches(beta: &str, ci: &str, also: &[&str]) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let mut ci_uses: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for job in workflows::job_names(ci)?.into_iter().filter(|j| j != JOB) {
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

/// Every job of `text` with a `--release` command before its `actions/cache/save` step: a
/// release profile build adds a second set of artifacts to `target`, which the cache save then
/// has to write (it filled the runner's disk). An error when the text does not parse.
fn release_before_cache_save(text: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    for job in workflows::job_names(text)? {
        if workflows::job_uses(text, &job)?.is_some() {
            continue;
        }
        let mut release = false;
        for step in workflows::job_steps(text, &job)? {
            if step.run.as_deref().is_some_and(|r| r.contains("--release")) {
                release = true;
            }
            let saves = step
                .uses
                .as_deref()
                .is_some_and(|u| u.starts_with("actions/cache/save@"));
            if saves && release {
                found.push(format!(
                    "{BETA}: job {job} builds --release before it saves the cache"
                ));
            }
        }
    }
    Ok(found)
}

#[test]
fn no_job_builds_a_release_profile_before_it_saves_the_cargo_cache() {
    // Catches: a `--release` step placed before the cache save of a job, which grows `target`
    // past the runner's disk while the cache is written.
    let root = super::repo_root();
    let text = std::fs::read_to_string(root.join(BETA)).expect(BETA);
    assert_eq!(release_before_cache_save(&text), Ok(Vec::new()), "{BETA}");
    let planted = "on: push\npermissions: {}\njobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo test --release\n      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n";
    let found = release_before_cache_save(planted).expect("parses");
    assert_eq!(found.len(), 1, "{found:?}");
    let after = "on: push\npermissions: {}\njobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n      - run: cargo test --release\n";
    assert_eq!(release_before_cache_save(after), Ok(Vec::new()));
    assert!(release_before_cache_save("jobs: [").is_err());
}

#[test]
fn the_smoke_job_copies_the_action_commits_of_the_other_jobs() {
    // Catches: an action bump in another job of beta.yml or artifacts.yml that leaves the smoke job on
    // the old commit.
    let root = super::repo_root();
    let read = |path: &str| std::fs::read_to_string(root.join(path)).expect(path);
    let artifacts = read(".github/workflows/artifacts.yml");
    assert_eq!(
        mismatches(&read(SMOKE), &read(BETA), &[&artifacts]),
        Ok(Vec::new())
    );
}

/// The rules [`mismatches`] reads the two workflows by, on planted ones.
mod rules {
    use super::*;

    const SHA_A: &str = "3d3c42e5aac5ba805825da76410c181273ba90b1";
    const SHA_B: &str = "55cc8345863c7cc4c66a329aec7e433d2d1c52a9";

    /// A workflow of one job `job` with `steps`.
    fn workflow(job: &str, steps: &str) -> String {
        format!(
            "on: push\npermissions: {{}}\njobs:\n  {job}:\n    runs-on: ubuntu-24.04\n    steps:\n{steps}"
        )
    }

    fn uses(name: &str, sha: &str) -> String {
        format!("      - uses: {name}@{sha} # v1\n")
    }

    #[test]
    fn equal_commits_pass_and_other_jobs_may_use_actions_the_smoke_job_does_not() {
        // Catches: the rule refusing what CI is: an action used by several jobs at one commit,
        // and actions the smoke job does not use.
        let beta = workflow("smoke", &uses("actions/checkout", SHA_A));
        let ci = format!(
            "{}{}",
            workflow(
                "test",
                &(uses("actions/checkout", SHA_A) + &uses("actions/x", SHA_B))
            ),
            "  other:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v1\n"
        );
        assert_eq!(mismatches(&beta, &ci, &[]), Ok(Vec::new()));
    }

    #[test]
    fn each_drift_is_named() {
        // Catches: each way the copy goes stale accepted: an action's commit bumped on one side,
        // and an action only the smoke job uses.
        let ci = workflow("test", &uses("actions/checkout", SHA_A));
        let beta = workflow("smoke", &uses("actions/checkout", SHA_B));
        let found = mismatches(&beta, &ci, &[]).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("actions/checkout is at") && found[0].contains(SHA_A),
            "{found:?}"
        );

        let beta = workflow("smoke", &uses("actions/new", SHA_A));
        let found = mismatches(&beta, &ci, &[]).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("actions/new is used"), "{found:?}");
    }

    #[test]
    fn an_action_another_workflow_uses_counts_as_used_by_another_job() {
        // Catches: the artifacts workflow's actions (whose files the smoke downloads) not
        // counting, which would refuse an action only it and the smoke job use.
        let ci = workflow("test", &uses("actions/checkout", SHA_A));
        let beta = workflow("smoke", &uses("actions/download-artifact", SHA_B));
        let artifacts = workflow("build", &uses("actions/download-artifact", SHA_B));
        assert_eq!(mismatches(&beta, &ci, &[&artifacts]), Ok(Vec::new()));
        assert_eq!(mismatches(&beta, &ci, &[]).expect("parses").len(), 1);
    }

    #[test]
    fn a_ci_job_that_disagrees_is_named_even_when_another_agrees() {
        // Catches: the first job that matches settling it (a second job uses the action at a
        // stale commit).
        let ci = format!(
            "{}{}",
            workflow("a", &uses("actions/checkout", SHA_A)),
            "  b:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/checkout@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v1\n"
        );
        let beta = workflow("smoke", &uses("actions/checkout", SHA_A));
        let found = mismatches(&beta, &ci, &[]).expect("both parse");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains(SHA_B), "{found:?}");
    }

    #[test]
    fn a_workflow_that_does_not_parse_or_lacks_the_job_is_an_error_not_a_pass() {
        // Catches: a lint that passes because it read nothing.
        assert!(mismatches("jobs: [", "on: push", &[]).is_err());
        let ci = workflow("test", "");
        assert!(mismatches(&ci, &ci, &[]).is_err(), "no smoke job");
    }
}
