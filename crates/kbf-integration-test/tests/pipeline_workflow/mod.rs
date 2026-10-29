//! What `.github/workflows/pipeline.yml` and the workflows it calls must keep: the one
//! pipeline of a push to main (docs/design/kbf-cicd.md, section 8), as workflow rules.
//! [`problems`] reads the workflow files and reports each way they differ from the rules; the
//! real files must report none, and `tests` plants a defect for each rule.
//!
//! - **One pipeline.** `pipeline.yml` runs on a push to `branches: [main]` and on nothing
//!   else, with the jobs [`JOBS`]. `ci`, `beta`, `artifacts` and `smoke` call `ci.yml`,
//!   `beta.yml`, `artifacts.yml` and `smoke.yml`. `ci` and `beta` need nothing and start
//!   together; `artifacts` needs `ci`, and `smoke`, the one job that tests the artifacts, needs
//!   `artifacts` (a commit that fails the gate is neither signed nor smoke-tested). No stage
//!   can pass or run after a red one, so a red or cancelled stage stops the stages after it.
//! - **Everything must pass.** `passed` needs every stage and has no `if:`, so the run is green
//!   only when each stage is, and the release (which follows the run's success) is made only
//!   then. It is also the end of the run for `watch`.
//! - **Fail fast.** `watch` needs nothing, runs only for a push to main (the one condition the
//!   workflow lint lets hold `actions: write`), builds xtask before any step holds a token and
//!   ends with the one step that does: `cargo xtask pipeline-watch`, which cancels the run as soon
//!   as a job has failed.
//! - **Each check once.** The four called workflows have a `workflow_call` trigger and no
//!   `push` trigger; `ci.yml` and `artifacts.yml` keep `pull_request` and `merge_group`;
//!   `beta.yml` and `smoke.yml` have `workflow_call` alone. Exactly one job in the repository
//!   calls each of them, and it is the job of the pipeline, so a push to main runs each check
//!   once.
//! - **Smoke tests the artifacts of the same run; beta needs none.** `smoke.yml` downloads the
//!   `assets-*` and `sha256sums` artifacts without a `run-id` (the run it is part of), checks
//!   them with `verify-assets sums`, takes the `kbf` of the linux-x86_64 tarball and runs the
//!   farm from it; the job that runs the farm builds no `kbf` of its own. `beta.yml` downloads
//!   no artifact, which is what lets it start beside `ci`.
//! - **A running pipeline is not cancelled by a push; a release is never cancelled.** The
//!   pipeline's concurrency is `pipeline-${{ github.ref }}` with `cancel-in-progress: false`: the
//!   run in progress is never cancelled by a push, and of the pushes that wait GitHub keeps the
//!   newest. Nothing in it releases (no environment, no secret, no token of the release bot). The
//!   release is another workflow, in a group of its own that never cancels (`release_workflow`
//!   holds that rule), started by the success of a run of this one.
//! - **Least privilege.** Every job has `contents: read` and nothing else but the signing
//!   scopes of `artifacts`, the `actions: write` of `watch`, and the `actions: read` of `ci`
//!   (the coverage parts wait for each other's artifacts); a called workflow gets at most
//!   what its caller job grants.

use super::workflows;

/// The pipeline's `name:`, which `release.yml` follows.
const NAME: &str = "pipeline";

/// The jobs, in file order.
const JOBS: [&str; 6] = ["ci", "beta", "artifacts", "smoke", "watch", "passed"];

/// `(job, workflow file, the job it needs)`: the stages, in order.
const STAGES: [(&str, &str, Option<&str>); 4] = [
    ("ci", "ci.yml", None),
    ("beta", "beta.yml", None),
    ("artifacts", "artifacts.yml", Some("ci")),
    ("smoke", "smoke.yml", Some("artifacts")),
];

/// The only job that holds a write scope: the call of `artifacts.yml`, whose attest jobs sign.
const SIGNING_JOB: &str = "artifacts";

/// Its `if:`: a push to `main`, the exact spelling the signing scopes need (`workflows`).
pub const SIGNING_IF: &str =
    "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}";

/// The fail-fast job: it cancels the run when another job has failed, and is the one job that
/// holds a write scope besides the signing ones (`actions: write`, to cancel this run).
const WATCH: &str = "watch";

/// The closing job: it needs every stage and does nothing else, so it completes (skipped, when a
/// stage did not pass) only when the run is over. The watch ends when it has.
const PASSED: &str = "passed";

/// The command of the watch's only step that holds a token.
const WATCH_COMMAND: &str = "cargo xtask pipeline-watch --repo \"$GITHUB_REPOSITORY\" --run-id \"$GITHUB_RUN_ID\" --last-job passed";

/// The group and the cancel of the pipeline: a newer push to the ref replaces the run.
const GROUP: &str = "pipeline-${{ github.ref }}";

/// Status functions and keys that let a stage pass or run after a red one.
const CONTINUING: &[&str] = &["always()", "failure()", "cancelled()", "continue-on-error"];

/// The workflow files, by role.
pub struct Files {
    pub pipeline: String,
    pub ci: String,
    pub artifacts: String,
    pub beta: String,
    pub smoke: String,
    pub release: String,
    /// Every other workflow file, `(name, text)`.
    pub others: Vec<(String, String)>,
}

impl Files {
    fn called(&self, file: &str) -> &str {
        match file {
            "ci.yml" => &self.ci,
            "artifacts.yml" => &self.artifacts,
            "smoke.yml" => &self.smoke,
            _ => &self.beta,
        }
    }
}

fn input<'a>(step: &'a workflows::Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

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

/// What differs between the pipeline's files and the rules above: one line per difference,
/// empty when there is none. An error when a workflow cannot be read.
pub fn problems(files: &Files) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    pipeline(files, &mut out)?;
    called(files, &mut out)?;
    each_check_once(files, &mut out)?;
    beta_needs_nothing_of_the_run(&files.beta, &mut out);
    smoke_consumes_the_run(&files.smoke, &mut out)?;
    watcher(&files.pipeline, &mut out)?;
    release_follows(files, &mut out)?;
    Ok(out)
}

fn pipeline(files: &Files, out: &mut Vec<String>) -> Result<(), String> {
    let text = &files.pipeline;
    if name_of(text) != NAME {
        out.push(format!(
            "pipeline.yml is named {:?}, not `{NAME}`: the release follows that name",
            name_of(text)
        ));
    }
    let events = workflows::events(text)?;
    if events != ["push"] {
        out.push(format!(
            "pipeline.yml runs on {events:?}, not on `push` alone: only a push to main runs the pipeline"
        ));
    }
    let filters = workflows::push_filters(text).unwrap_or_default();
    if filters != [("branches".to_owned(), vec!["main".to_owned()])] {
        out.push(format!(
            "`push` is filtered by {filters:?}, not exactly `branches: [main]`"
        ));
    }
    let names = workflows::job_names(text)?;
    if names != JOBS {
        out.push(format!(
            "the pipeline's jobs are {names:?}, not {JOBS:?} in that order"
        ));
    }
    let concurrency = workflows::concurrency(text)?;
    if concurrency != Some((GROUP.to_owned(), "false".to_owned())) {
        out.push(format!(
            "the pipeline's concurrency is {concurrency:?}, not `group: {GROUP}` with `cancel-in-progress: false`: a running pipeline always finishes, and only the newest waiting push queues behind it"
        ));
    }
    for (job, file, needs) in STAGES {
        let uses = workflows::job_uses(text, job)?;
        let want = format!("./.github/workflows/{file}");
        if uses.as_deref() != Some(want.as_str()) {
            out.push(format!("the job `{job}` calls {uses:?}, not `{want}`"));
        }
        let have = workflows::job_needs(text, job)?;
        let want_needs: Vec<String> = needs.iter().map(|n| (*n).to_owned()).collect();
        if have != want_needs {
            out.push(format!(
                "the job `{job}` needs {have:?}, not {want_needs:?}: each stage needs the one before it"
            ));
        }
        let steps = workflows::job_steps(text, job);
        if steps.is_ok() {
            out.push(format!(
                "the job `{job}` has steps: a stage of the pipeline only calls its workflow"
            ));
        }
        let condition = workflows::job_condition(text, job)?;
        let want_if = (job == SIGNING_JOB).then_some(SIGNING_IF);
        if condition.as_deref() != want_if {
            out.push(format!(
                "the job `{job}` has `if: {}`, not {}: a stage that can be skipped lets the later stages and the release run without it",
                condition.as_deref().unwrap_or("(none)"),
                want_if.map_or("none".to_owned(), |c| format!("exactly `{c}`"))
            ));
        }
    }
    permissions(text, out)?;
    callee_scopes(files, out)?;
    no_secrets_passed(text, out)?;
    let all = code(text);
    for word in CONTINUING {
        if all.contains(word) {
            out.push(format!(
                "pipeline.yml uses `{word}`: a red or cancelled stage would not stop the stages after it"
            ));
        }
    }
    for word in [
        "environment:",
        "secrets.",
        "RELEASE_BOT",
        "create-github-app-token",
        "gh release",
        "release tag",
    ] {
        if all.contains(word) {
            out.push(format!(
                "pipeline.yml names `{word}`: nothing that releases may be in the pipeline"
            ));
        }
    }
    Ok(())
}

/// Least privilege: the scopes each job of the pipeline grants its called workflow.
fn permissions(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    for job in JOBS {
        let have = workflows::job_permission_levels(text, job)?;
        let want: Vec<(&str, &str)> = match job {
            SIGNING_JOB => vec![
                ("contents", "read"),
                ("id-token", "write"),
                ("attestations", "write"),
            ],
            "beta" => vec![("contents", "read"), ("attestations", "read")],
            // The last part of the coverage run lists the run's jobs and artifacts.
            "ci" => vec![("contents", "read"), ("actions", "read")],
            WATCH => vec![("contents", "read"), ("actions", "write")],
            _ => vec![("contents", "read")],
        };
        let mut got: Vec<String> = have.iter().map(|(s, l)| format!("{s}: {l}")).collect();
        let mut wanted: Vec<String> = want.iter().map(|(s, l)| format!("{s}: {l}")).collect();
        got.sort();
        wanted.sort();
        if got != wanted {
            out.push(format!(
                "the job `{job}` grants the scopes {got:?}, not exactly {wanted:?}"
            ));
        }
    }
    Ok(())
}

/// How much a permission level allows: `write` includes `read`; an unknown level allows
/// nothing it can be compared with, so it counts as the most.
fn level_rank(level: &str) -> u8 {
    match level {
        "none" => 0,
        "read" => 1,
        "write" => 2,
        _ => 3,
    }
}

/// A called workflow gets at most what its caller job grants: GitHub refuses the whole
/// calling workflow (a startup failure, no job runs) when a job of the called one asks for
/// more. Checks every job of each called workflow against the grant of the job that calls it.
fn callee_scopes(files: &Files, out: &mut Vec<String>) -> Result<(), String> {
    for (stage, file, _) in STAGES {
        let grant = workflows::job_permission_levels(&files.pipeline, stage)?;
        let text = files.called(file);
        for job in workflows::job_names(text)? {
            for (scope, level) in workflows::job_permission_levels(text, &job)? {
                let granted = grant
                    .iter()
                    .find(|(s, _)| *s == scope)
                    .map_or("none", |(_, l)| l.as_str());
                if level_rank(&level) > level_rank(granted) {
                    out.push(format!(
                        "the job `{job}` of {file} asks for `{scope}: {level}`, but the job `{stage}` of pipeline.yml that calls it grants `{scope}: {granted}`: a called workflow cannot exceed its caller, and GitHub would refuse the whole pipeline at startup"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// No stage call passes secrets (`secrets: inherit` or a list): the release bot's key is
/// readable only by the release workflow, and a run a newer push cancels holds none.
fn no_secrets_passed(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    for (stage, _, _) in STAGES {
        if workflows::job_has_key(text, stage, "secrets")? {
            out.push(format!(
                "the job `{stage}` passes `secrets:` to its workflow: no stage may read a secret"
            ));
        }
    }
    Ok(())
}

fn has(events: &[String], event: &str) -> bool {
    events.iter().any(|e| e == event)
}

/// The called workflows: callable, not started by a push, and still run for pull requests.
fn called(files: &Files, out: &mut Vec<String>) -> Result<(), String> {
    for (_, file, _) in STAGES {
        let text = files.called(file);
        let events = workflows::events(text)?;
        if !has(&events, "workflow_call") {
            out.push(format!(
                "{file} has no `workflow_call` trigger: the pipeline could not call it"
            ));
        }
        if has(&events, "push") {
            out.push(format!("{file} has a `push` trigger: a push to main would run it beside the pipeline, and each check must run once"));
        }
        let pull_requests = !matches!(file, "beta.yml" | "smoke.yml");
        for event in ["pull_request", "merge_group"] {
            if pull_requests && !has(&events, event) {
                out.push(format!("{file} does not run on `{event}`: the pull-request gate and the merge queue keep running it"));
            }
            if !pull_requests && has(&events, event) {
                out.push(format!("{file} runs on `{event}`: beta runs after the artifacts of a push to main, and nowhere else"));
            }
        }
        if let Some((group, _)) = workflows::concurrency(text)?
            && group.starts_with("pipeline-")
        {
            out.push(format!("{file} has the concurrency group `{group}`, which the pipeline's own group would cancel: a called workflow's group never repeats its caller's"));
        }
    }
    Ok(())
}

/// Exactly one job in the repository calls each stage workflow, and it is the pipeline's.
fn each_check_once(files: &Files, out: &mut Vec<String>) -> Result<(), String> {
    let mut all: Vec<(&str, &str)> = vec![
        ("pipeline.yml", &files.pipeline),
        ("ci.yml", &files.ci),
        ("artifacts.yml", &files.artifacts),
        ("beta.yml", &files.beta),
        ("smoke.yml", &files.smoke),
        ("release.yml", &files.release),
    ];
    all.extend(files.others.iter().map(|(n, t)| (n.as_str(), t.as_str())));
    for (_, file, _) in STAGES {
        let needle = format!("uses: ./.github/workflows/{file}");
        let callers: Vec<&str> = all
            .iter()
            .filter(|(_, t)| code(t).lines().any(|l| l.trim() == needle))
            .map(|(n, _)| *n)
            .collect();
        if callers != ["pipeline.yml"] {
            out.push(format!(
                "{file} is called by {callers:?}, not by `pipeline.yml` alone: a push to main must run each check once"
            ));
        }
    }
    Ok(())
}

/// The smoke runs the `kbf` of the linux-x86_64 tarball of this run's artifacts.
fn smoke_consumes_the_run(smoke: &str, out: &mut Vec<String>) -> Result<(), String> {
    if code(smoke).contains("run-id") {
        out.push(
            "smoke.yml names a `run-id`: it reads the artifacts of the run it is part of"
                .to_owned(),
        );
    }
    let steps = workflows::job_steps(smoke, "smoke")?;
    let downloads: Vec<&workflows::Step> = steps
        .iter()
        .filter(|s| {
            s.uses
                .as_deref()
                .is_some_and(|u| u.starts_with("actions/download-artifact@"))
        })
        .collect();
    let assets = downloads
        .iter()
        .position(|d| input(d, "pattern") == Some("assets-*") && input(d, "path") == Some("dist"));
    let sums = downloads
        .iter()
        .position(|d| input(d, "name") == Some("sha256sums") && input(d, "path") == Some("dist"));
    if assets.is_none() {
        out.push(
            "the job `smoke` of smoke.yml does not download `assets-*` into `dist`".to_owned(),
        );
    }
    if sums.is_none() {
        out.push(
            "the job `smoke` of smoke.yml does not download `sha256sums` into `dist`".to_owned(),
        );
    }
    let exact = |command: &str| {
        steps
            .iter()
            .position(|s| s.run.as_deref().is_some_and(|r| r.trim() == command))
    };
    let checked = exact("cargo xtask verify-assets sums dist");
    let asset_checked = exact(
        "cargo xtask build-commit check-asset \"dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz\" \"$GITHUB_SHA\"",
    );
    let extracted =
        exact("tar -xzf \"dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz\" -C \"$RUNNER_TEMP/bin\" kbf");
    let ran = steps.iter().position(|s| {
        s.run
            .as_deref()
            .is_some_and(|r| r.starts_with("target/debug/kbf-beta b0 "))
    });
    if checked.is_none() {
        out.push(
            "the job `smoke` does not check the downloaded files with `verify-assets sums dist`"
                .to_owned(),
        );
    }
    if asset_checked.is_none() {
        out.push("the job `smoke` does not run `build-commit check-asset` on the linux-x86_64 tarball for this commit: a kbf of another commit or a debug build could be the one tested".to_owned());
    }
    match (checked, asset_checked, extracted, ran) {
        (Some(c), Some(a), Some(e), Some(r)) if c < a && a < e && e < r => {}
        _ => out.push("the job `smoke` does not check the files, then check the tarball's kbf with `build-commit check-asset`, then take `kbf` out of the linux-x86_64 tarball into `$RUNNER_TEMP/bin`, then run the farm, in that order".to_owned()),
    }
    if let Some(r) = ran
        && !steps[r]
            .run
            .as_deref()
            .unwrap_or_default()
            .contains("--bin-dir \"$RUNNER_TEMP/bin\"")
    {
        out.push("the farm of `smoke` does not run from `--bin-dir \"$RUNNER_TEMP/bin\"`, where the tarball's kbf is".to_owned());
    }
    for s in &steps {
        let run = s.run.as_deref().unwrap_or_default();
        if run.starts_with("cargo build") && (run.contains("kbf-cli") || run.contains("--bin kbf "))
        {
            out.push(
                "the job `smoke` builds `kbf`: the farm must run the artifact of this run"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// `beta.yml` tests the commit's source and waits for nothing: it downloads no artifact of the
/// run (a job that does belongs in `smoke.yml`, behind `artifacts`), so it can start with `ci`.
fn beta_needs_nothing_of_the_run(beta: &str, out: &mut Vec<String>) {
    if code(beta).contains("download-artifact") {
        out.push("beta.yml downloads an artifact: it starts beside `ci`, before any artifact exists; a job that tests the run's artifacts belongs in smoke.yml".to_owned());
    }
}

/// The fail-fast and the end of the run: `passed` needs every stage, `watch` cancels the run
/// from a token it holds in its last step alone, under the push-to-main condition.
fn watcher(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let stages: Vec<String> = STAGES.iter().map(|(job, _, _)| (*job).to_owned()).collect();
    let needs = workflows::job_needs(text, PASSED)?;
    if needs != stages {
        out.push(format!(
            "the job `{PASSED}` needs {needs:?}, not {stages:?}: it is green only when every stage is, and the release follows the run's success"
        ));
    }
    if let Some(condition) = workflows::job_condition(text, PASSED)? {
        out.push(format!(
            "the job `{PASSED}` has `if: {condition}`: a skipped closing job would let the run succeed without a stage"
        ));
    }
    let needs = workflows::job_needs(text, WATCH)?;
    if !needs.is_empty() {
        out.push(format!(
            "the job `{WATCH}` needs {needs:?}: it must run beside the stages to cancel them"
        ));
    }
    let condition = workflows::job_condition(text, WATCH)?;
    if condition.as_deref() != Some(SIGNING_IF) {
        out.push(format!(
            "the job `{WATCH}` has `if: {}`, not exactly `{SIGNING_IF}`: the lint lets only that condition hold `actions: write`",
            condition.as_deref().unwrap_or("(none)")
        ));
    }
    let steps = workflows::job_steps(text, WATCH)?;
    let at = steps
        .iter()
        .position(|s| s.run.as_deref() == Some(WATCH_COMMAND));
    let Some(at) = at else {
        out.push(format!(
            "the job `{WATCH}` does not run `{WATCH_COMMAND}`: nothing cancels the run when a job fails"
        ));
        return Ok(());
    };
    if steps[at].env != [("GH_TOKEN".to_owned(), "${{ github.token }}".to_owned())] {
        out.push(format!(
            "the step of `{WATCH}` that cancels does not have exactly `GH_TOKEN: ${{{{ github.token }}}}` as its environment"
        ));
    }
    for (i, step) in steps.iter().enumerate() {
        let token = step.env.iter().any(|(_, v)| v.contains("token"))
            || step.with.iter().any(|(_, v)| v.contains("token"));
        if i != at && token {
            out.push(format!(
                "a step of `{WATCH}` other than the one that cancels holds a token: the build of xtask must run before any step holds one"
            ));
        }
        if step
            .run
            .as_deref()
            .is_some_and(|r| r.starts_with("cargo build"))
            && i > at
        {
            out.push(format!(
                "the job `{WATCH}` builds after the step that holds the token: no build script may run beside it"
            ));
        }
    }
    if at + 1 != steps.len() {
        out.push(format!(
            "the step of `{WATCH}` that cancels is not its last: a step after it would run beside the token"
        ));
    }
    Ok(())
}

/// The release follows this pipeline and finds what it left.
fn release_follows(files: &Files, out: &mut Vec<String>) -> Result<(), String> {
    let filters = workflows::trigger_filters(&files.release, "workflow_run").unwrap_or_default();
    let named = filters
        .iter()
        .find(|(k, _)| k == "workflows")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if named != [NAME] {
        out.push(format!(
            "release.yml follows {named:?}, not the pipeline `{NAME}`"
        ));
    }
    let release_group = workflows::concurrency(&files.release)?.map(|(g, _)| g);
    if release_group.is_none_or(|g| g.starts_with("pipeline-")) {
        out.push("release.yml shares the pipeline's concurrency group: a push that cancels the pipeline would cancel a release".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
