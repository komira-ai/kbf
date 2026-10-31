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

/// The publish program, run from the commit's own source (Node strips the types), and the two files the deploy job checks out.
const PROGRAM: &str = "crates/tierstore/site/tools/publish.ts";
const PUBLISH_PATHS: [&str; 2] = [
    "crates/tierstore/site/.node-version",
    "crates/tierstore/site/tools/publish.ts",
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
    if let Some(i) = upload {
        let path = steps[i]
            .with
            .iter()
            .find(|(k, _)| k == "path")
            .map(|(_, v)| v.as_str());
        if path != Some("crates/tierstore/site/dist") {
            out.push(format!("the build job's artifact path is {path:?}, not exactly `crates/tierstore/site/dist`: the artifact carries the pages only, never a program the deploy job would run"));
        }
    }
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
    let mut perms = levels(text, "deploy")?;
    perms.sort();
    if perms
        != [
            ("contents".to_owned(), "read".to_owned()),
            ("id-token".to_owned(), "write".to_owned()),
        ]
    {
        out.push(format!("the deploy job's permissions are {perms:?}, not `contents: read` (for the checkout) and `id-token: write` alone: the job needs no other token right"));
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
    // Exactly: the program's source from the commit itself, the Node it pins, the built pages, the one command.
    let kinds: Vec<String> = steps
        .iter()
        .map(|s| match (&s.uses, &s.run) {
            (Some(u), _) => u.split('@').next().unwrap_or_default().to_owned(),
            (None, Some(_)) => "run".to_owned(),
            _ => "?".to_owned(),
        })
        .collect();
    if kinds
        != [
            "actions/checkout",
            "actions/setup-node",
            "actions/download-artifact",
            "run",
        ]
    {
        out.push(format!("the deploy job's steps are {kinds:?}, not exactly a checkout, a setup-node, a download of the pages and the one publish command: the program that holds the credentials comes from the commit's own source, and nothing is installed or built beside the token"));
        return Ok(());
    }
    let with = |i: usize, key: &str| {
        steps[i]
            .with
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let want = |i: usize, key: &str, value: &str, out: &mut Vec<String>| {
        if with(i, key).as_deref() != Some(value) {
            out.push(format!(
                "the deploy job's step {} has `{key}` {:?}, not `{value}`",
                i + 1,
                with(i, key)
            ));
        }
    };
    want(0, "ref", "${{ github.sha }}", out);
    want(0, "persist-credentials", "false", out);
    want(0, "sparse-checkout-cone-mode", "false", out);
    let sparse = with(0, "sparse-checkout").unwrap_or_default();
    let mut paths: Vec<&str> = sparse
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    paths.sort();
    if paths != PUBLISH_PATHS {
        out.push(format!("the deploy job's checkout takes {paths:?}, not exactly {PUBLISH_PATHS:?}: only the publish program and the Node version file"));
    }
    want(1, "node-version-file", PUBLISH_PATHS[0], out);
    want(2, "name", "tierstore-site", out);
    want(2, "path", "dist", out);
    let run = steps[3].run.as_deref().unwrap_or_default();
    if !run.starts_with(&format!("node {PROGRAM} ")) || !run.contains("--dist dist ") {
        out.push(format!("the deploy job runs `{run}`: its only command is `node {PROGRAM} --dist dist ...`, run from the checked-out source"));
    }
    publish_step(&steps[3], out);
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

/// The module specifiers `src` (TypeScript source) imports or requires, one per use, comment lines skipped.
fn specifiers(src: &str) -> Vec<String> {
    let quoted = |rest: &str| -> Option<String> {
        let start = rest.find(['\'', '"'])?;
        let quote = rest[start..].chars().next()?;
        let body = &rest[start + 1..];
        Some(body[..body.find(quote)?].to_owned())
    };
    let mut out = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') || t.starts_with("/*") {
            continue;
        }
        let from = t
            .find(" from ")
            .filter(|_| t.starts_with("import ") || t.starts_with("export ") || t.starts_with('}'));
        match from {
            Some(at) => out.extend(quoted(&t[at..])),
            None if t.starts_with("} from ") => out.extend(quoted(t)),
            None if t.starts_with("import ") && !t.contains('(') => out.extend(quoted(t)),
            None => {}
        }
        for call in ["require(", "import("] {
            let mut rest = t;
            while let Some(i) = rest.find(call) {
                rest = &rest[i + call.len()..];
                out.extend(quoted(rest));
            }
        }
    }
    out
}

/// The program that holds the credentials runs from source with no install: it may import `node:` built-ins and nothing else,
/// and its syntax must be what Node's type stripping erases (the site's tsconfig forbids the rest).
pub fn source_problems(src: &str, tsconfig: &str) -> Vec<String> {
    let mut out = Vec::new();
    let found = specifiers(src);
    if found.is_empty() {
        out.push("publish.ts imports nothing: the scan read nothing".to_owned());
    }
    for s in found.iter().filter(|s| !s.starts_with("node:")) {
        out.push(format!("publish.ts imports `{s}`: only `node:` built-ins, because the deploy job runs it with no install and no other file"));
    }
    if !tsconfig.contains("\"erasableSyntaxOnly\": true") {
        out.push("tsconfig.json lacks `erasableSyntaxOnly: true`: an enum, namespace or parameter property in publish.ts would stop Node from running it as it is".to_owned());
    }
    out
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
