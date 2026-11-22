//! The macOS `kbf-task` bundle on the registry: the job `task-bundle` of
//! `.github/workflows/release.yml`.
//!
//! - **When.** Under [`super::RELEASE_IF`], after `publish`, needing `plan` and `verify` too: it
//!   pushes the bundle of a release that is published, from the files `verify` checked.
//! - **What it holds.** `contents: read` and `packages: write`, and no other scope: it is the one
//!   job that may write packages (the graph's rule), and it names no environment and no secret.
//! - **What it runs.** Exactly [`RUNS`]: the toolchain, the build of xtask before any step holds
//!   the token, the check of the files against `SHA256SUMS`, and `push-task-bundle` of this
//!   commit's bundle asset to `ghcr.io/<owner>/kbf-task`, tagged with the planned tag, its
//!   reference written to the run's summary. Only that last step holds the token, `GH_TOKEN` of
//!   the job, beside `TAG`, and nothing else in its `env:`.
//! - **What it reads.** Its checkout and one download of the artifact `release-assets` into
//!   `dist`, of this run (no `run-id`): no other action.

use super::{
    ASSETS_ARTIFACT, DIST, NAME_KEY, PUBLISH, Rule, env_is, files_are_checked, flag_is, input,
    lines, run_of, uses, workflows,
};

/// The job that pushes the bundle.
pub const JOB: &str = "task-bundle";

/// The verb it runs.
const VERB: &str = "cargo xtask push-task-bundle ";

/// Every `run:` of the job, in order.
const RUNS: [&str; 4] = [
    "rustup toolchain install",
    "cargo build -p xtask --locked",
    "sha256sum --check --strict SHA256SUMS",
    "cargo xtask push-task-bundle --asset \"dist/kbf-task-bundle-$COMMIT-darwin-arm64.tar.gz\" --repository \"ghcr.io/$GITHUB_REPOSITORY_OWNER/kbf-task\" --tag \"$TAG\" --summary \"$GITHUB_STEP_SUMMARY\"",
];

/// The actions the job may use: its checkout and the download of the checked files.
const ACTIONS: [&str; 2] = ["actions/checkout@", "actions/download-artifact@"];

/// The row of the job in the graph.
pub fn rule() -> Rule {
    super::rule(
        JOB,
        &["plan", "verify", PUBLISH],
        &[("contents", "read"), ("packages", "write")],
        super::RELEASE_IF,
        None,
    )
}

/// What differs between the job and the rules above.
pub fn problems(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, JOB)?;
    let runs: Vec<String> = steps
        .iter()
        .filter(|s| s.run.is_some())
        .flat_map(|s| lines(s).map(str::to_owned).collect::<Vec<_>>())
        .collect();
    if runs != RUNS {
        out.push(format!(
            "the job `{JOB}` runs {runs:?}, not exactly {RUNS:?}"
        ));
    }
    let mut downloads = 0;
    for step in &steps {
        let Some(action) = step.uses.as_deref() else {
            continue;
        };
        if !ACTIONS.iter().any(|a| action.starts_with(a)) {
            out.push(format!(
                "the job `{JOB}` uses `{action}`: it reads the checked files and nothing else"
            ));
        }
        if uses(step, "actions/download-artifact@") {
            downloads += 1;
            if input(step, NAME_KEY) != Some(ASSETS_ARTIFACT) || input(step, "path") != Some(DIST) {
                out.push(format!(
                    "the download of `{JOB}` is not of `{ASSETS_ARTIFACT}` into `{DIST}`: it pushes the files `verify` checked"
                ));
            }
        }
    }
    if downloads != 1 {
        out.push(format!(
            "the job `{JOB}` has {downloads} downloads, not one"
        ));
    }
    files_are_checked(JOB, &steps, out);
    for step in &steps {
        let token = step.env.iter().any(|(k, _)| k == "GH_TOKEN");
        let pushes = step
            .run
            .as_deref()
            .is_some_and(|r| r.trim_start().starts_with(VERB));
        if token && !pushes {
            out.push(format!(
                "a step of `{JOB}` that does not push holds `GH_TOKEN`: only the push may"
            ));
        }
    }
    let verb = VERB.trim_end();
    if let Some((at, step, words)) = run_of(JOB, &steps, VERB, out) {
        flag_is(
            &words,
            verb,
            "--asset",
            "dist/kbf-task-bundle-$COMMIT-darwin-arm64.tar.gz",
            out,
        );
        flag_is(
            &words,
            verb,
            "--repository",
            "ghcr.io/$GITHUB_REPOSITORY_OWNER/kbf-task",
            out,
        );
        flag_is(&words, verb, "--tag", "$TAG", out);
        flag_is(&words, verb, "--summary", "$GITHUB_STEP_SUMMARY", out);
        env_is(JOB, step, "TAG", "${{ needs.plan.outputs.tag }}", out);
        env_is(JOB, step, "GH_TOKEN", "${{ github.token }}", out);
        if step.env.len() != 2 {
            out.push(format!(
                "the step of `{JOB}` that runs `{verb}` sets {:?}, not exactly `TAG` and `GH_TOKEN`",
                step.env.iter().map(|(k, _)| k).collect::<Vec<_>>()
            ));
        }
        let checked_before = steps[..at].iter().any(|s| {
            s.run
                .as_deref()
                .is_some_and(|r| r.trim() == "sha256sum --check --strict SHA256SUMS")
        });
        if !checked_before {
            out.push(format!(
                "the job `{JOB}` pushes before it checks the files against SHA256SUMS"
            ));
        }
    }
    for job in workflows::job_names(release)? {
        if job == JOB {
            continue;
        }
        let holds = workflows::job_permission_levels(release, &job)?
            .iter()
            .any(|(scope, _)| scope == "packages");
        if holds {
            out.push(format!(
                "the job `{job}` names the `packages` scope: only `{JOB}` may"
            ));
        }
    }
    Ok(())
}
