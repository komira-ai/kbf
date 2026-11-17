//! What `.github/workflows/suites.yml` must keep: the source suite of a push to main, run beside
//! the pipeline instead of in it, and the issue that keeps its red. [`problems`] reads the
//! workflow and reports each way it differs from the rules; the real file must report none, and
//! `tests` changes the real file for each rule.
//!
//! - **One trigger.** `suites.yml` is named `suites`, runs on a push to `branches: [main]` and on
//!   nothing else, and has `permissions: {}` at the top.
//! - **Never cancelled.** Its concurrency group is exactly `suites-${{ github.ref }}` with
//!   `cancel-in-progress: false`: a suite that has started reports, and the newest waiting push
//!   queues behind it.
//! - **Two jobs.** [`JOBS`], in order. `beta` calls `./.github/workflows/beta.yml` with no `if:`,
//!   no `needs`, no secrets and exactly the scopes `contents: read` and `attestations: read`: a
//!   skipped or conditional call would not report, and the called jobs may ask for no more.
//! - **The issue is kept whatever the suite did.** `release-red` needs exactly `beta`, runs under
//!   exactly [`RELEASE_RED_IF`] (the one condition the workflow lint lets hold `issues: write`),
//!   and holds exactly `contents: read`, `actions: read`, `checks: read` and `issues: write`.
//! - **The token is held by the last step alone.** The last step runs exactly [`COMMAND`] with
//!   exactly `GH_TOKEN: ${{ github.token }}` as its environment; no earlier step holds a token or
//!   a secret; and `cargo build -p xtask --locked` runs before it, so no build script runs
//!   beside the token.

use super::workflows;

/// The workflow's `name:`.
const NAME: &str = "suites";

/// The jobs, in file order.
const JOBS: [&str; 2] = ["beta", "release-red"];

/// The group and the cancel of the suite.
const GROUP: &str = "suites-${{ github.ref }}";

/// The workflow the job `beta` calls.
const BETA_USES: &str = "./.github/workflows/beta.yml";

/// The `if:` of `release-red`: it runs after a red, skipped or cancelled suite too, and only for
/// `main`'s ref.
const RELEASE_RED_IF: &str = "${{ always() && github.ref == 'refs/heads/main' }}";

/// The command of the last step of `release-red`.
const COMMAND: &str =
    "cargo xtask release-red --repo \"$GITHUB_REPOSITORY\" --run-id \"$GITHUB_RUN_ID\"";

/// The build that must precede [`COMMAND`].
const BUILD: &str = "cargo build -p xtask --locked";

/// The scopes of each job, `(job, scopes)`.
const SCOPES: [(&str, &[(&str, &str)]); 2] = [
    ("beta", &[("contents", "read"), ("attestations", "read")]),
    (
        "release-red",
        &[
            ("contents", "read"),
            ("actions", "read"),
            ("checks", "read"),
            ("issues", "write"),
        ],
    ),
];

/// The lines of a workflow that are not comments.
fn code(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn name_of(text: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix("name: "))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// What differs between `suites.yml` and the rules above: one line per difference, empty when
/// there is none. An error when the workflow cannot be read.
pub fn problems(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    trigger(text, &mut out)?;
    jobs(text, &mut out)?;
    beta_call(text, &mut out)?;
    release_red(text, &mut out)?;
    Ok(out)
}

fn trigger(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    if name_of(text) != NAME {
        out.push(format!(
            "suites.yml is named {:?}, not `{NAME}`",
            name_of(text)
        ));
    }
    let events = workflows::events(text)?;
    if events != ["push"] {
        out.push(format!(
            "suites.yml runs on {events:?}, not on `push` alone: only a push to main runs the suite"
        ));
    }
    let filters = workflows::push_filters(text).unwrap_or_default();
    if filters != [("branches".to_owned(), vec!["main".to_owned()])] {
        out.push(format!(
            "`push` is filtered by {filters:?}, not exactly `branches: [main]`"
        ));
    }
    if !code(text).lines().any(|l| l == "permissions: {}") {
        out.push("suites.yml has no top-level `permissions: {}`: the jobs would start with the default token rights".to_owned());
    }
    let concurrency = workflows::concurrency(text)?;
    if concurrency != Some((GROUP.to_owned(), "false".to_owned())) {
        out.push(format!(
            "the concurrency of suites.yml is {concurrency:?}, not `group: {GROUP}` with `cancel-in-progress: false`: a suite that has started always reports"
        ));
    }
    Ok(())
}

fn jobs(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let names = workflows::job_names(text)?;
    if names != JOBS {
        out.push(format!(
            "the jobs of suites.yml are {names:?}, not {JOBS:?} in that order"
        ));
    }
    for (job, want) in SCOPES {
        // A job that is missing is already reported above.
        let Ok(have) = workflows::job_permission_levels(text, job) else {
            continue;
        };
        let mut got: Vec<String> = have.iter().map(|(s, l)| format!("{s}: {l}")).collect();
        let mut wanted: Vec<String> = want.iter().map(|(s, l)| format!("{s}: {l}")).collect();
        got.sort();
        wanted.sort();
        if got != wanted {
            out.push(format!(
                "the job `{job}` of suites.yml grants the scopes {got:?}, not exactly {wanted:?}"
            ));
        }
    }
    Ok(())
}

/// `beta` calls beta.yml and nothing else: no condition, no wait, no secret.
fn beta_call(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let uses = workflows::job_uses(text, "beta")?;
    if uses.as_deref() != Some(BETA_USES) {
        out.push(format!(
            "the job `beta` of suites.yml calls {uses:?}, not `{BETA_USES}`"
        ));
    }
    if let Some(c) = workflows::job_condition(text, "beta")? {
        out.push(format!(
            "the job `beta` of suites.yml has `if: {c}`: a skipped suite would report nothing"
        ));
    }
    let needs = workflows::job_needs(text, "beta")?;
    if !needs.is_empty() {
        out.push(format!(
            "the job `beta` of suites.yml needs {needs:?}: the suite waits for nothing"
        ));
    }
    if workflows::job_has_key(text, "beta", "secrets")? {
        out.push(
            "the job `beta` of suites.yml passes `secrets:` to beta.yml: the suite reads no secret"
                .to_owned(),
        );
    }
    if workflows::job_steps(text, "beta").is_ok() {
        out.push("the job `beta` of suites.yml has steps: it only calls beta.yml".to_owned());
    }
    Ok(())
}

/// `release-red` keeps the issue after the suite, red or green, with a token that only its last
/// step holds.
fn release_red(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    const JOB: &str = "release-red";
    let needs = workflows::job_needs(text, JOB)?;
    if needs != ["beta"] {
        out.push(format!(
            "the job `{JOB}` needs {needs:?}, not [\"beta\"]: it reads the result of the suite"
        ));
    }
    let condition = workflows::job_condition(text, JOB)?;
    if condition.as_deref() != Some(RELEASE_RED_IF) {
        out.push(format!(
            "the job `{JOB}` has `if: {}`, not exactly `{RELEASE_RED_IF}`: it must run after a red suite, and only for main, the one condition the workflow lint lets hold `issues: write`",
            condition.as_deref().unwrap_or("(none)")
        ));
    }
    let steps = workflows::job_steps(text, JOB)?;
    let Some(at) = steps.len().checked_sub(1) else {
        out.push(format!("the job `{JOB}` has no steps"));
        return Ok(());
    };
    if steps[at].run.as_deref() != Some(COMMAND) {
        out.push(format!(
            "the last step of `{JOB}` does not run `{COMMAND}`: nothing keeps the RELEASE-RED issue"
        ));
    }
    if steps[at].env != [("GH_TOKEN".to_owned(), "${{ github.token }}".to_owned())] {
        out.push(format!(
            "the last step of `{JOB}` does not have exactly `GH_TOKEN: ${{{{ github.token }}}}` as its environment"
        ));
    }
    let mut built = false;
    for step in &steps[..at] {
        let holds = |kv: &[(String, String)]| {
            kv.iter()
                .any(|(_, v)| v.contains("token") || v.contains("secrets."))
        };
        if holds(&step.env) || holds(&step.with) {
            out.push(format!(
                "a step of `{JOB}` before the last holds a token or a secret: the build of xtask must run before any step holds one"
            ));
        }
        if step.run.as_deref() == Some(BUILD) {
            built = true;
        }
    }
    if !built {
        out.push(format!(
            "the job `{JOB}` does not run `{BUILD}` before its last step: the token would be held while xtask builds"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
