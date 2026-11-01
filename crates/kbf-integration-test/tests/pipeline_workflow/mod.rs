//! What `.github/workflows/pipeline.yml` and the workflows it calls must keep: the one
//! pipeline of a push to main (docs/design/kbf-cicd.md, section 8), as workflow rules.
//! [`problems`] reads the workflow files and reports each way they differ from the rules; the
//! real files must report none, and `tests` plants a defect for each rule.
//!
//! - **One pipeline.** `pipeline.yml` runs on a push to `branches: [main]` and on nothing
//!   else, with the jobs [`JOBS`]. `ci`, `beta`, `artifacts`, `artifacts-sign` and `smoke` call
//!   `ci.yml`, `beta.yml`, `artifacts.yml`, `artifacts.yml` and `smoke.yml`. `ci`, `beta`
//!   and `artifacts` (the builds, `with: stage: build`) need nothing and start together;
//!   `artifacts-sign` (the attest and verify jobs, `with: stage: sign`) needs `ci` and
//!   `artifacts`, and `smoke`, the one job that tests the artifacts, needs `artifacts-sign` (a
//!   commit that fails the gate is built but neither signed nor smoke-tested). No stage can pass
//!   or run after a red one, so a red or cancelled stage stops the stages after it.
//! - **One signer file, two calls.** The signer identity of an attestation is the workflow file,
//!   `artifacts.yml`, and the installers verify exactly it, so the attest jobs stay there. In
//!   `artifacts.yml` every job that holds a signing scope has exactly [`SIGNING_STAGE_IF`], every
//!   `verify-*` job has it too, and every other job's `if:` says `inputs.stage != 'sign'`: the sign
//!   call runs nothing else and the build call runs none of them. A job under [`SIGNING_STAGE_IF`]
//!   needs only jobs under it (a skipped `need` would skip it and let an unsigned commit pass).
//! - **Everything must pass.** `passed` needs every stage and has no `if:`, so the run is green
//!   only when each stage is, and the release (which follows the run's success) is made only
//!   then. It is also the end of the run for `watch`.
//! - **Fail fast.** `watch` needs nothing, runs only for a push to main (the one condition the
//!   workflow lint lets hold `actions: write`), builds xtask before any step holds a token and
//!   ends with the one step that does: `cargo xtask pipeline-watch`, which cancels the run as soon
//!   as a job has failed.
//! - **Each check once.** The four called workflows have a `workflow_call` trigger and no
//!   `push` trigger; `ci.yml` and `artifacts.yml` keep `pull_request` and `merge_group`;
//!   `beta.yml` and `smoke.yml` have `workflow_call` alone. The jobs that call them are the
//!   pipeline's alone: one job each, but for `artifacts.yml`, which `artifacts` (stage build) and
//!   `artifacts-sign` (stage sign) call, so a push to main runs each check once.
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
//!   scopes of the two calls of `artifacts.yml` (the build call has them only because a called
//!   workflow is checked against its caller's grant statically; its signing jobs are skipped),
//!   the `actions: read` of `ci` (its `test` job waits for the coverage shards' artifacts) and
//!   the `actions: write` of `watch`; a called workflow gets at most
//!   what its caller job grants.

use super::workflows;

/// The pipeline's `name:`, which `release.yml` follows.
const NAME: &str = "pipeline";

/// The jobs, in file order.
const JOBS: [&str; 7] = [
    "ci",
    "beta",
    "artifacts",
    "artifacts-sign",
    "smoke",
    "watch",
    "passed",
];

/// `(job, workflow file, the jobs it needs)`: the stages, in order.
const STAGES: [(&str, &str, &[&str]); 5] = [
    ("ci", "ci.yml", &[]),
    ("beta", "beta.yml", &[]),
    ("artifacts", "artifacts.yml", &[]),
    ("artifacts-sign", "artifacts.yml", &["ci", "artifacts"]),
    ("smoke", "smoke.yml", &["artifacts-sign"]),
];

/// The call of `artifacts.yml` that builds (`stage: build`): it starts with `ci` and signs
/// nothing: every job of artifacts.yml that holds a signing scope is skipped in it
/// ([`artifacts_stages`]). It is still granted the same scopes as the sign call, because GitHub
/// checks a called workflow's job permissions against the caller's grant statically, skipped
/// jobs included (`callee_scopes`).
const BUILD_JOB: &str = "artifacts";

/// The call of `artifacts.yml` that signs (`stage: sign`): the call in which the attest jobs of
/// `artifacts.yml` run.
const SIGNING_JOB: &str = "artifacts-sign";

/// The groups that are signed, `(group, the sums job that publishes its SHA256SUMS)`. The build
/// call's sums job hands the digest of the sums it uploaded to the sign call, whose attest job of
/// the group refuses other sums (`attested_bundles` holds that check). The workflow output and the
/// input of the sign call are both named `sums_<group>_sha256` (see `attested_bundles::digest_input`).
const SIGNED_GROUPS: [(&str, &str); 2] = [
    ("linux", "sums-linux"),
    ("darwin-arm64", "sums-darwin-arm64"),
];

/// The `with:` of the build call: its stage alone.
fn build_with() -> Vec<(String, String)> {
    vec![("stage".to_owned(), "build".to_owned())]
}

/// The `with:` of the sign call: its stage, and the build call's digest of each group's sums.
fn sign_with() -> Vec<(String, String)> {
    let mut with = vec![("stage".to_owned(), "sign".to_owned())];
    for (group, _) in SIGNED_GROUPS {
        let name = super::attested_bundles::digest_input(group);
        with.push((
            name.clone(),
            format!("${{{{ needs.{BUILD_JOB}.outputs.{name} }}}}"),
        ));
    }
    with
}

/// The `if:` of the two calls and of `watch`: a push to `main`, the exact spelling the signing
/// scopes need (`workflows`; a caller job has no `inputs`, so this is not the stage spelling).
pub const SIGNING_IF: &str =
    "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}";

/// The `if:` of every job of `artifacts.yml` that signs or verifies: the push to `main` in the
/// sign stage (the spelling the workflow lint accepts for the signing scopes in a called workflow).
const SIGNING_STAGE_IF: &str = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && inputs.stage == 'sign' }}";

/// What every other job of `artifacts.yml` must say in its `if:`, so the sign call runs none of them.
const NOT_SIGN_STAGE: &str = "inputs.stage != 'sign'";

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
    artifacts_stages(&files.artifacts, &mut out)?;
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
                "the job `{job}` needs {have:?}, not {want_needs:?}: the builds start with ci, signing needs ci and the build call, and smoke needs the signing call"
            ));
        }
        let steps = workflows::job_steps(text, job);
        if steps.is_ok() {
            out.push(format!(
                "the job `{job}` has steps: a stage of the pipeline only calls its workflow"
            ));
        }
        let condition = workflows::job_condition(text, job)?;
        let want_if = (job == BUILD_JOB || job == SIGNING_JOB).then_some(SIGNING_IF);
        if condition.as_deref() != want_if {
            out.push(format!(
                "the job `{job}` has `if: {}`, not {}: a stage that can be skipped lets the later stages and the release run without it",
                condition.as_deref().unwrap_or("(none)"),
                want_if.map_or("none".to_owned(), |c| format!("exactly `{c}`"))
            ));
        }
    }
    for (job, want) in [(BUILD_JOB, build_with()), (SIGNING_JOB, sign_with())] {
        let with = workflows::job_with(text, job)?;
        if with != want {
            out.push(format!(
                "the job `{job}` passes {with:?}, not exactly {want:?}: the two calls of artifacts.yml differ by their stage, and the sign call is given the digests of the sums the build call published (its attest jobs refuse other sums)"
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
            BUILD_JOB | SIGNING_JOB => vec![
                ("contents", "read"),
                ("id-token", "write"),
                ("attestations", "write"),
            ],
            "beta" => vec![("contents", "read"), ("attestations", "read")],
            // The `test` job of ci.yml lists the run's jobs and artifacts, to wait for the shards.
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
    let mut called_files: Vec<&str> = STAGES.iter().map(|(_, file, _)| *file).collect();
    called_files.dedup();
    for file in called_files {
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
    let mut files_called: Vec<&str> = STAGES.iter().map(|(_, file, _)| *file).collect();
    files_called.dedup();
    for file in files_called {
        let needle = format!("uses: ./.github/workflows/{file}");
        // The pipeline's jobs that call it: one each, except artifacts.yml, which two call.
        let wanted: Vec<&str> = STAGES
            .iter()
            .filter(|(_, f, _)| *f == file)
            .map(|(job, _, _)| *job)
            .collect();
        let mut callers: Vec<String> = Vec::new();
        for (name, text) in &all {
            for job in workflows::job_names(text).unwrap_or_default() {
                if workflows::job_uses(text, &job).ok().flatten().as_deref()
                    == Some(&needle["uses: ".len()..])
                {
                    callers.push(format!("{name}#{job}"));
                }
            }
        }
        let want: Vec<String> = wanted.iter().map(|j| format!("pipeline.yml#{j}")).collect();
        if callers != want {
            out.push(format!(
                "{file} is called by {callers:?}, not by {want:?} alone: a push to main must run each check once"
            ));
        }
    }
    Ok(())
}

/// The two calls of `artifacts.yml` split its jobs: the sign call runs the jobs that sign or verify
/// and nothing else, the build call (and a pull request or the merge queue) runs the rest and
/// never those. Every job that holds a signing scope, and every `verify-*` job, has exactly
/// [`SIGNING_STAGE_IF`]; every other job's `if:` says [`NOT_SIGN_STAGE`], so the sign call
/// rebuilds nothing. A job under [`SIGNING_STAGE_IF`] needs only jobs under it: a skipped `need`
/// would skip the signing job and let an unsigned commit through as a success.
fn artifacts_stages(artifacts: &str, out: &mut Vec<String>) -> Result<(), String> {
    digests_handed_on(artifacts, out)?;
    let names = workflows::job_names(artifacts)?;
    let mut staged: Vec<(String, String)> = Vec::new();
    for job in &names {
        let signs = workflows::job_permission_levels(artifacts, job)?
            .iter()
            .any(|(s, l)| matches!(s.as_str(), "id-token" | "attestations") && l == "write");
        let condition = workflows::job_condition(artifacts, job)?;
        let shown = condition.as_deref().unwrap_or("(none)");
        if signs || job.starts_with("verify-") {
            if condition.as_deref() != Some(SIGNING_STAGE_IF) {
                out.push(format!(
                    "the job `{job}` of artifacts.yml {} has `if: {shown}`, not exactly `{SIGNING_STAGE_IF}`: only the sign call may run it",
                    if signs { "holds a signing scope and" } else { "verifies and" }
                ));
            }
            for need in workflows::job_needs(artifacts, job)? {
                staged.push((job.clone(), need));
            }
        } else if !condition.as_deref().is_some_and(excludes_sign_stage) {
            out.push(format!(
                "the job `{job}` of artifacts.yml has `if: {shown}`, not `${{{{ (<condition>) && {NOT_SIGN_STAGE} }}}}`: the sign call would run it again"
            ));
        }
    }
    for (job, need) in staged {
        if workflows::job_condition(artifacts, &need)?.as_deref() != Some(SIGNING_STAGE_IF) {
            out.push(format!(
                "the job `{job}` of artifacts.yml needs `{need}`, which the sign call skips: `{job}` would be skipped and nothing signed"
            ));
        }
    }
    Ok(())
}

/// Whether `condition` is exactly `${{ (<condition>) && inputs.stage != 'sign' }}`, the
/// parenthesis closing at the end of `<condition>`: `x || inputs.stage != 'sign'`, or
/// `(a) || (b) && inputs.stage != 'sign'` (where `&&` binds first), would still let the sign
/// call run the job.
fn excludes_sign_stage(condition: &str) -> bool {
    let tail = format!(" && {NOT_SIGN_STAGE} }}}}");
    let Some(inner) = condition
        .strip_prefix("${{ ")
        .and_then(|rest| rest.strip_suffix(tail.as_str()))
    else {
        return false;
    };
    let Some(body) = inner
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    // The opening parenthesis closes at the end: the body never closes more than it opens.
    let mut depth = 0i32;
    for c in body.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// The digest of each group's SHA256SUMS travels from the build call to the sign call: the sums
/// job of the group writes `sha256=<hex>` of the file it uploads to its step output (`sums
/// assemble --digest-output "$GITHUB_OUTPUT"`) and exposes it as the job output `sha256`;
/// `workflow_call` re-exports it as the output `sums_<group>_sha256` and declares the input of the
/// same name, which the sign call's attest job reads. (The pipeline passes it: [`pipeline`].)
fn digests_handed_on(artifacts: &str, out: &mut Vec<String>) -> Result<(), String> {
    for (group, sums_job) in SIGNED_GROUPS {
        let name = super::attested_bundles::digest_input(group);
        let outputs = workflows::job_outputs(artifacts, sums_job)?;
        if outputs
            != [(
                "sha256".to_owned(),
                "${{ steps.assemble.outputs.sha256 }}".to_owned(),
            )]
        {
            out.push(format!(
                "the job `{sums_job}` of artifacts.yml has the outputs {outputs:?}, not exactly `sha256: ${{{{ steps.assemble.outputs.sha256 }}}}`: the sign call could not tell the sums it signs from others"
            ));
        }
        let steps = workflows::job_steps(artifacts, sums_job)?;
        let assembles = steps.iter().any(|s| {
            s.id.as_deref() == Some("assemble")
                && !s.conditional
                && s.run.as_deref().is_some_and(|r| {
                    r.starts_with("cargo xtask sums assemble dist")
                        && r.contains("--digest-output \"$GITHUB_OUTPUT\"")
                })
        });
        if !assembles {
            out.push(format!(
                "the job `{sums_job}` of artifacts.yml has no unconditional step `assemble` that runs `cargo xtask sums assemble dist ... --digest-output \"$GITHUB_OUTPUT\"`: its output `sha256` would be empty or not the digest of the uploaded sums"
            ));
        }
        let code = code(artifacts);
        let export = format!("value: ${{{{ jobs.{sums_job}.outputs.sha256 }}}}");
        let declared = |section: &str| {
            let at = code.find(&format!("    {section}:\n"))?;
            let rest = &code[at + 1..];
            // To the next key of the same indentation (or the end of `workflow_call`).
            let end = rest
                .match_indices("\n    ")
                .find(|(i, _)| rest[i + 5..].chars().next().is_some_and(|c| c != ' '))
                .map_or_else(
                    || rest.find("\npermissions:").unwrap_or(rest.len()),
                    |(i, _)| i,
                );
            Some(&rest[..end])
        };
        let outputs_section = declared("outputs").unwrap_or_default();
        if !outputs_section.contains(&format!("      {name}:\n"))
            || !outputs_section.contains(&export)
        {
            out.push(format!(
                "artifacts.yml's `workflow_call` does not declare the output `{name}` with `{export}`: the pipeline could not hand the digest to the sign call"
            ));
        }
        if !declared("inputs")
            .unwrap_or_default()
            .contains(&format!("      {name}:\n"))
        {
            out.push(format!(
                "artifacts.yml's `workflow_call` does not declare the input `{name}`: the sign call could not be given the digest"
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
