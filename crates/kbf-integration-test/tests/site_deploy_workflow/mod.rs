//! What `.github/workflows/tierstore-site-publish.yml` must keep: the deploy of tierstore.dev holds
//! cloud access, so its safety properties are rules here, and the real file must report no problem.
//! [`problems`] reads the workflow text; `tests` plants a defect for each rule.
//!
//! - **Main only.** It runs on `push` (branch `main` and paths, nothing else) and `workflow_dispatch`, and
//!   on no other event: no pull request of any kind, no schedule, no tag.
//! - **One job holds the token.** The jobs are `build` and `deploy`. `build` has `contents: read` and nothing
//!   else, no environment and no `needs`; `deploy` has `id-token: write` and nothing else. The workflow lint
//!   (`workflows`) additionally allows that scope only under the deploy's exact `if:` and environment.
//! - **A protected environment.** `deploy` runs in `prod-site-tierstore`, the environment the AWS role trusts and whose
//!   reviewers and branch rule protect the settings; its `if:` is exactly [`DEPLOY_IF`] (a push or a manual run, on
//!   `main`) and it `needs` the build.
//! - **No stored key.** The role, the bucket and the distribution come from the environment's variables
//!   (`vars.`), through the step's `env:` and the program's flags. No `secrets.` context and no AWS key
//!   variable appears anywhere in the file.
//! - **Built and tested first.** `build` installs with scripts off, type-checks, runs the tests and builds, in that order,
//!   and only then uploads the artifact; no step of either job is conditional or may fail without failing the job. `deploy`
//!   downloads that artifact and ends with the one `run:` step, the publish program. It checks out nothing and
//!   runs no package manager, so no repository code beside the token but the program that was built and tested.
//! - **A deploy is never cancelled.** The workflow's concurrency group is fixed and `cancel-in-progress` is `false`.

use super::workflows;

/// The workflow file, relative to the repository root.
pub const FILE: &str = ".github/workflows/tierstore-site-publish.yml";

/// The environment of the deploy job (the AWS role's trust names it).
const ENVIRONMENT: &str = "prod-site-tierstore";

/// The deploy job's only `if:`: a push or a manual run, on the ref `main`.
const DEPLOY_IF: &str = "${{ (github.event_name == 'push' || github.event_name == 'workflow_dispatch') && github.ref == 'refs/heads/main' }}";

/// `(variable of the step's env, repository variable)`: where the publish program's flags come from.
const SETTINGS: [(&str, &str); 3] = [
    ("BUCKET", "TIERSTORE_SITE_BUCKET"),
    ("DISTRIBUTION_ID", "TIERSTORE_SITE_DISTRIBUTION_ID"),
    ("ROLE_ARN", "TIERSTORE_SITE_PUBLISH_ROLE_ARN"),
];

/// The build job's commands, in the order they must run before anything is uploaded.
const BUILD_RUNS: [&str; 4] = [
    "npm ci --ignore-scripts --no-audit --no-fund",
    "npm run typecheck",
    "npm test",
    "npm run build",
];

/// Spellings that mean a stored cloud key.
const KEY_WORDS: [&str; 4] = [
    "secrets.",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "aws-access-key-id",
];

/// One message per way `text` (the workflow) differs from the rules. An error text is a problem too.
pub fn problems(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Err(e) = rules(text, &mut out) {
        out.push(format!("{FILE} could not be read: {e}"));
    }
    out
}

fn rules(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    triggers(text, out)?;
    let jobs = workflows::job_names(text)?;
    if jobs != ["build", "deploy"] {
        out.push(format!("the jobs are {jobs:?}, not [\"build\", \"deploy\"]: the token and the build must stay in separate jobs"));
    }
    for key in ["build", "deploy"] {
        if !jobs.iter().any(|j| j == key) {
            return Ok(());
        }
    }
    build_job(text, out)?;
    deploy_job(text, out)?;
    no_stored_key(text, out);
    concurrency(text, out)?;
    Ok(())
}

fn triggers(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let mut events = workflows::events(text)?;
    events.sort();
    if events != ["push", "workflow_dispatch"] {
        out.push(format!("the workflow runs on {events:?}, not on `push` and `workflow_dispatch` alone: a pull request, a schedule or a tag must never reach the deploy"));
    }
    let filters = workflows::push_filters(text)?;
    let branches = filters
        .iter()
        .find(|(k, _)| k == "branches")
        .map(|(_, v)| v.clone());
    if branches.as_deref() != Some(&["main".to_owned()][..]) {
        out.push(format!(
            "the push trigger has branches {branches:?}, not exactly [\"main\"]"
        ));
    }
    if let Some((k, _)) = filters
        .iter()
        .find(|(k, _)| k != "branches" && k != "paths")
    {
        out.push(format!(
            "the push trigger has the filter `{k}`: only `branches` and `paths` are allowed"
        ));
    }
    Ok(())
}

fn levels(text: &str, job: &str) -> Result<Vec<(String, String)>, String> {
    workflows::job_permission_levels(text, job)
}

fn build_job(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let perms = levels(text, "build")?;
    if perms != [("contents".to_owned(), "read".to_owned())] {
        out.push(format!("the build job's permissions are {perms:?}, not `contents: read` alone: no token that signs in a job that runs the repository's code"));
    }
    if workflows::job_environment(text, "build")?.is_some() {
        out.push("the build job has an environment: the settings and the role belong to the deploy job alone".to_owned());
    }
    if !workflows::job_needs(text, "build")?.is_empty() {
        out.push(
            "the build job needs another job: it must start first and hold nothing".to_owned(),
        );
    }
    let steps = workflows::job_steps(text, "build")?;
    conditional(&steps, "build", out);
    // The commands, in order: each must come after the one before it.
    let mut at = 0;
    for want in BUILD_RUNS {
        match steps
            .iter()
            .enumerate()
            .skip(at)
            .find(|(_, s)| s.run.as_deref() == Some(want))
        {
            Some((i, _)) => at = i + 1,
            None => {
                out.push(format!("the build job does not run `{want}` (in the order {BUILD_RUNS:?}) before its upload"));
                return Ok(());
            }
        }
    }
    let upload = steps.iter().position(|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/upload-artifact@"))
    });
    match upload {
        Some(i) if i >= at => {}
        _ => out.push(
            "the build job's artifact upload is missing or comes before the build and the tests"
                .to_owned(),
        ),
    }
    Ok(())
}

fn deploy_job(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let perms = levels(text, "deploy")?;
    if perms != [("id-token".to_owned(), "write".to_owned())] {
        out.push(format!("the deploy job's permissions are {perms:?}, not `id-token: write` alone: the job needs no repository or other token right"));
    }
    let environment = workflows::job_environment(text, "deploy")?;
    if environment.as_deref() != Some(ENVIRONMENT) {
        out.push(format!("the deploy job's environment is {environment:?}, not `{ENVIRONMENT}`: the role's trust, the variables and the reviewers belong to that environment"));
    }
    let condition = workflows::job_condition(text, "deploy")?;
    if condition.as_deref() != Some(DEPLOY_IF) {
        out.push(format!(
            "the deploy job's `if:` is {condition:?}, not exactly `{DEPLOY_IF}`"
        ));
    }
    if workflows::job_needs(text, "deploy")? != ["build"] {
        out.push("the deploy job does not need exactly the build job: nothing is uploaded before the tests pass".to_owned());
    }
    let steps = workflows::job_steps(text, "deploy")?;
    conditional(&steps, "deploy", out);
    if steps.iter().any(|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/checkout@"))
    }) {
        out.push("the deploy job checks out the repository: it may run only the program the build job built".to_owned());
    }
    for s in steps.iter().filter_map(|s| s.run.as_deref()) {
        let first = s.split_whitespace().next().unwrap_or_default();
        if !s.starts_with("node publish/") || matches!(first, "npm" | "npx" | "cargo" | "pip") {
            out.push(format!("the deploy job runs `{s}`: its only command is the publish program (`node publish/...`)"));
        }
    }
    let runs: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.run.is_some())
        .map(|(i, _)| i)
        .collect();
    let download = steps.iter().position(|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/download-artifact@"))
    });
    match (runs.as_slice(), download) {
        ([run], Some(d)) if *run == steps.len() - 1 && d < *run => publish_step(&steps[*run], out),
        _ => out.push("the deploy job must download the artifact and then end with exactly one `run:` step, the publish program".to_owned()),
    }
    Ok(())
}

fn publish_step(step: &workflows::Step, out: &mut Vec<String>) {
    let run = step.run.as_deref().unwrap_or_default();
    for (variable, setting) in SETTINGS {
        let want = format!("${{{{ vars.{setting} }}}}");
        match step.env.iter().find(|(k, _)| k == variable) {
            Some((_, v)) if *v == want => {}
            other => out.push(format!("the publish step's {variable} is {:?}, not `{want}`: the role, the bucket and the distribution come from the environment's variables", other.map(|(_, v)| v))),
        }
    }
    if step.env.len() != SETTINGS.len() {
        out.push("the publish step's env holds a variable beyond the three settings".to_owned());
    }
    for flag in [
        "--bucket \"$BUCKET\"",
        "--distribution-id \"$DISTRIBUTION_ID\"",
        "--role-arn \"$ROLE_ARN\"",
    ] {
        if !run.contains(flag) {
            out.push(format!("the publish command lacks `{flag}`: the setting must reach the program as that flag"));
        }
    }
}

/// A step with an `if:` or `continue-on-error:` could be skipped, or fail, with the job still green.
fn conditional(steps: &[workflows::Step], job: &str, out: &mut Vec<String>) {
    for (i, s) in steps.iter().enumerate() {
        if s.conditional {
            out.push(format!("step {} of the {job} job has an `if:` or `continue-on-error:`: a skipped or failing step would leave the job green", i + 1));
        }
    }
}

fn no_stored_key(text: &str, out: &mut Vec<String>) {
    let code = text.lines().filter(|l| !l.trim_start().starts_with('#'));
    for line in code {
        for word in KEY_WORDS {
            if line.contains(word) {
                out.push(format!("`{}` names `{word}`: the deploy holds no stored key and no secret, only the OIDC token and variables", line.trim()));
            }
        }
    }
    if workflows::top_env(text).is_ok_and(|e| !e.is_empty()) {
        out.push(
            "the workflow has a top-level `env:`: it would reach every step of both jobs"
                .to_owned(),
        );
    }
}

fn concurrency(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    match workflows::concurrency(text)? {
        Some((group, cancel)) if cancel == "false" && !group.contains("${{") && !group.is_empty() => {}
        other => out.push(format!("the concurrency is {other:?}, not a fixed group with `cancel-in-progress: false`: a deploy that has started must finish, and two never overlap")),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
