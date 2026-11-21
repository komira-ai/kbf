//! The fail-fast of the pipeline, run as the workflow file declares it: for the results of the
//! stages, which cancel jobs would run, and what their one step asks `gh` to do. The real
//! `pipeline.yml` is read; the status function and the step's `run:` line are evaluated here
//! (no shell, no process, no network), and the `gh` that receives the command is a fake that
//! records it. The text rules are in `tests`; these tests hold the behaviour they protect.

use std::collections::BTreeMap;

use super::*;

const RUN_ID: &str = "4242";
const REPO: &str = "owner/repo";
const TOKEN: &str = "token-of-this-run";

/// The result of a job a cancel job needs, as GitHub names it.
const RESULTS: [&str; 4] = ["success", "failure", "cancelled", "skipped"];

fn pipeline_text() -> String {
    std::fs::read_to_string(crate::repo_root().join(".github/workflows/pipeline.yml"))
        .expect("pipeline.yml")
}

/// Whether a job with `condition` and `needs` runs, given the result of each stage and the event
/// and ref of the run. Only the one condition the lint allows is understood; any other is a
/// failure of the test, not a guess.
fn runs(
    condition: &str,
    needs: &[String],
    results: &BTreeMap<&str, &str>,
    event: &str,
    git_ref: &str,
) -> bool {
    assert_eq!(
        condition, CANCEL_IF,
        "a condition this evaluator does not know"
    );
    // `failure()`: a job it needs has failed. A skipped or cancelled need is not a failure.
    let failure = needs.iter().any(|n| results[n.as_str()] == "failure");
    failure && event == "push" && git_ref == "refs/heads/main"
}

/// The words of a `run:` line, with `"..."` quoting and `$NAME` expanded from `env`. A name that
/// is not in `env` fails the test: the step would run a shell with a variable nobody set.
fn words(line: &str, env: &BTreeMap<&str, &str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    let mut started = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ' ' if !quoted => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '$' => {
                let mut name = String::new();
                while let Some(n) = chars
                    .peek()
                    .filter(|n| n.is_ascii_alphanumeric() || **n == '_')
                {
                    name.push(*n);
                    chars.next();
                }
                word.push_str(
                    env.get(name.as_str())
                        .unwrap_or_else(|| panic!("`${name}` is not set")),
                );
                started = true;
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    assert!(!quoted, "an unclosed quote in `{line}`");
    if started {
        out.push(word);
    }
    out
}

/// A `gh` that records what it is asked and the token it is given, and cancels only the run
/// it is asked to cancel.
#[derive(Default)]
struct FakeGh {
    cancelled: Vec<(String, String)>,
}

impl FakeGh {
    /// `gh run cancel <run id> --repo <repo>` with `GH_TOKEN`; anything else is refused.
    fn call(&mut self, argv: &[String], token: Option<&str>) -> Result<(), String> {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        if token != Some(TOKEN) {
            return Err(format!("no token: {token:?}"));
        }
        match args.as_slice() {
            ["gh", "run", "cancel", run, "--repo", repo] => {
                self.cancelled.push(((*repo).to_owned(), (*run).to_owned()));
                Ok(())
            }
            other => Err(format!("the fake gh does not know {other:?}")),
        }
    }
}

/// Runs the pipeline's cancel jobs for these stage results and returns the jobs that ran and
/// what the fake `gh` was asked to cancel.
fn drive(results: &BTreeMap<&str, &str>, event: &str, git_ref: &str) -> (Vec<String>, FakeGh) {
    let text = pipeline_text();
    let mut ran = Vec::new();
    let mut gh = FakeGh::default();
    for (job, _) in CANCELS {
        let needs = workflows::job_needs(&text, job).expect("needs");
        let condition = workflows::job_condition(&text, job)
            .expect("if")
            .expect("an if");
        if !runs(&condition, &needs, results, event, git_ref) {
            continue;
        }
        ran.push(job.to_owned());
        let steps = workflows::job_steps(&text, job).expect("steps");
        let [step] = steps.as_slice() else {
            panic!("{job} has {} steps", steps.len())
        };
        let mut env: BTreeMap<&str, &str> =
            BTreeMap::from([("GITHUB_RUN_ID", RUN_ID), ("GITHUB_REPOSITORY", REPO)]);
        // The token is what the step's own `env:` maps `github.token` to.
        let token = step
            .env
            .iter()
            .find(|(k, v)| k == "GH_TOKEN" && v == "${{ github.token }}")
            .map(|_| TOKEN);
        if let Some(t) = token {
            env.insert("GH_TOKEN", t);
        }
        let argv = words(step.run.as_deref().expect("a run step"), &env);
        gh.call(&argv, token)
            .unwrap_or_else(|e| panic!("{job}: {e}"));
    }
    (ran, gh)
}

fn stages(
    ci: &'static str,
    artifacts: &'static str,
    smoke: &'static str,
) -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("ci", ci),
        ("artifacts", artifacts),
        ("artifacts-sign", "skipped"),
        ("smoke", smoke),
    ])
}

#[test]
fn a_failed_stage_cancels_this_run_through_gh_and_only_this_run() {
    // Proves: when `ci` fails, `cancel-after-ci` runs and the step asks `gh run cancel` for this
    // run in this repository with the run's token; the same for `artifacts` and
    // `cancel-after-build`. The other cancel job does not run, so the run is cancelled once.
    let (ran, gh) = drive(
        &stages("failure", "success", "success"),
        "push",
        "refs/heads/main",
    );
    assert_eq!(ran, ["cancel-after-ci"]);
    assert_eq!(gh.cancelled, [(REPO.to_owned(), RUN_ID.to_owned())]);
    let (ran, gh) = drive(
        &stages("success", "failure", "skipped"),
        "push",
        "refs/heads/main",
    );
    assert_eq!(ran, ["cancel-after-build"]);
    assert_eq!(gh.cancelled, [(REPO.to_owned(), RUN_ID.to_owned())]);
    let (ran, gh) = drive(
        &stages("failure", "failure", "skipped"),
        "push",
        "refs/heads/main",
    );
    assert_eq!(ran, ["cancel-after-ci", "cancel-after-build"]);
    assert_eq!(gh.cancelled.len(), 2, "each asks for this run alone");
    assert!(
        gh.cancelled
            .iter()
            .all(|c| *c == (REPO.to_owned(), RUN_ID.to_owned()))
    );
}

#[test]
fn no_cancel_job_runs_for_a_stage_that_did_not_fail() {
    // Proves: a green stage, a skipped one and one cancelled from outside never cancel the run,
    // and a failed `smoke` (which no cancel job waits for) cancels nothing: the run ends by
    // itself.
    for result in RESULTS.into_iter().filter(|r| *r != "failure") {
        let (ran, gh) = drive(
            &stages(result, result, "success"),
            "push",
            "refs/heads/main",
        );
        assert!(ran.is_empty(), "{result}: {ran:?}");
        assert!(gh.cancelled.is_empty(), "{result}");
    }
    let (ran, gh) = drive(
        &stages("success", "success", "failure"),
        "push",
        "refs/heads/main",
    );
    assert!(ran.is_empty() && gh.cancelled.is_empty(), "{ran:?}");
}

#[test]
fn a_failed_stage_cancels_nothing_outside_a_push_to_main() {
    // Proves: the same failures cancel nothing for a pull request or for another ref, so the
    // token that can cancel is never used there.
    for (event, git_ref) in [
        ("pull_request", "refs/pull/1/merge"),
        ("push", "refs/heads/feature"),
        ("merge_group", "refs/heads/main"),
    ] {
        let (ran, gh) = drive(&stages("failure", "failure", "skipped"), event, git_ref);
        assert!(
            ran.is_empty() && gh.cancelled.is_empty(),
            "{event} {git_ref}"
        );
    }
}

#[test]
fn the_fake_gh_refuses_another_run_a_missing_token_and_another_command() {
    // Proves: the fake that the tests above rely on is strict, so they cannot pass through a
    // step that cancels something else, runs without the token, or runs another subcommand.
    let env = BTreeMap::from([("GITHUB_RUN_ID", RUN_ID), ("GITHUB_REPOSITORY", REPO)]);
    let mut gh = FakeGh::default();
    let good = words(
        "gh run cancel \"$GITHUB_RUN_ID\" --repo \"$GITHUB_REPOSITORY\"",
        &env,
    );
    assert_eq!(good, ["gh", "run", "cancel", RUN_ID, "--repo", REPO]);
    assert!(gh.call(&good, None).is_err());
    assert!(gh.call(&good, Some("other")).is_err());
    assert!(
        gh.call(&words("gh run rerun 1 --repo x", &env), Some(TOKEN))
            .is_err()
    );
    assert!(gh.call(&words("gh run cancel", &env), Some(TOKEN)).is_err());
    assert!(gh.cancelled.is_empty());
    gh.call(&good, Some(TOKEN)).expect("the real command");
    assert_eq!(gh.cancelled, [(REPO.to_owned(), RUN_ID.to_owned())]);
}

#[test]
#[should_panic(expected = "is not set")]
fn a_variable_the_step_does_not_have_fails_the_test() {
    // Proves: a step that names a variable nobody sets cannot pass as if it were expanded.
    words("gh run cancel \"$NOT_THERE\"", &BTreeMap::new());
}

#[test]
#[should_panic(expected = "unclosed quote")]
fn an_unclosed_quote_fails_the_test() {
    // Proves: a malformed `run:` line is an error and not a shorter command.
    words("gh run cancel \"1", &BTreeMap::new());
}

#[test]
#[should_panic(expected = "a condition this evaluator does not know")]
fn a_condition_other_than_the_pinned_one_fails_the_test() {
    // Proves: the evaluator does not guess at a condition the lint does not allow.
    runs(
        "${{ always() }}",
        &["ci".to_owned()],
        &stages("failure", "success", "success"),
        "push",
        "refs/heads/main",
    );
}
