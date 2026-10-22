//! What `.github/workflows/release.yml` must keep: the automatic release of
//! docs/design/kbf-cicd.md (section 8.1) as workflow rules. [`problems`] reads the workflow and
//! reports each way it differs; the real file must report none, and `tests` plants a defect for
//! each rule on it.
//!
//! - **Events.** Exactly [`EVENTS`]: `workflow_run` of the workflow `pipeline`, `completed`, for
//!   `branches: [main]` (the one use of the trigger in the repository: `workflows/workflow_run.rs`
//!   holds the rest of the rule). No dispatch, no push, no tag. `workflow_run` is refused unless
//!   every job of [`CHAIN`] is present: a pipeline run cuts a release only through the whole chain.
//!   Every job names the run that started it in its own `if:` or through `plan`: its conclusion is
//!   `success`, its event `push`, its branch `main` and its repository this one ([`PLAN_IF`]).
//! - **The graph.** The jobs [`JOBS`], in that order, each with exactly the `needs`, token
//!   scopes, `if:` and `environment` of [`RULES`]; the jobs that write need `plan` and `verify`,
//!   directly or through another. Only `publish` holds a write scope (`contents`), `id-token` is
//!   in no job ([`ID_TOKEN`]), and the jobs that tag, publish or bump run only under
//!   [`RELEASE_IF`]: a successful pipeline run for a push to `main` of code that is not a change
//!   to the pipeline.
//! - **Never cancelled, never queued behind a skipped run.** The concurrency group is `release`
//!   for a successful pipeline run and the run's own for any other (a cancelled or red pipeline
//!   run starts this workflow too, to skip it, and must not replace a release waiting in the
//!   group), and it never cancels.
//! - **Halt on red.** No `always()`, `failure()`, `cancelled()` or `continue-on-error`, and no
//!   step under `if:` but the ones of `notes` that read the previous release.
//! - **One commit.** The workflow sets `COMMIT` to the commit the pipeline run built and
//!   `PIPELINE_RUN` to its id; every checkout is of that commit (a `ref` of exactly that, no
//!   `repository` or `token`), and no expression reads `inputs.`, a pull request or `head_ref`,
//!   nor the name or message of the run that started this one outside the trigger condition.
//! - **Who holds the bot's key.** `RELEASE_BOT_*` and `actions/create-github-app-token` appear
//!   in `tag` and `bump` only, and both declare the `release` environment (so moving the key to
//!   an environment secret is a setting). `RELEASE_FREEZE` is read in `plan` only, and
//!   `RELEASE_TAGGER_*` nowhere. The token is minted after the build, scoped to one repository and
//!   the permissions the job needs, and only the step that writes holds it.
//! - **Data flow.** `plan` outputs the version, tag and class of the push, from the commit the
//!   pipeline run left for it; `verify` downloads the files of the pipeline run that started this
//!   one and no other; `tag` writes the record from that run and tags the planned tag at
//!   `COMMIT` with that record; `notes` checks the tag and writes the notes and the set hash;
//!   `publish` is given those, the planned tag and `COMMIT`; `bump` is given the planned version.
//!   Only `plan` and `verify` read another run's artifacts; every later job takes what `verify`
//!   checked. Each flag below is held to the value that makes it mean what its verb says.
//! - **The shared rules.** Actions pinned to a full commit SHA, hosted runners and an empty
//!   top-level `permissions` are the workflow lint's (`workflows`); they are repeated here so a
//!   defect planted in this file reads red from this test too.

use super::workflows;

/// The job that holds the write scope and creates the GitHub release.
const PUBLISH: &str = "publish";

/// The jobs, in file order.
const JOBS: [&str; 6] = ["plan", "verify", "tag", "notes", PUBLISH, "bump"];

/// The events that start the workflow: the completion of a run of [`PIPELINE`], filtered to
/// [`TYPES`] and `branches: [main]`, and only while [`CHAIN`] is whole.
const EVENTS: &[&str] = &["workflow_run"];

/// The workflow whose completion starts the release (its `name:`).
pub const PIPELINE: &str = "pipeline";

/// The activity types of the trigger.
const TYPES: &str = "completed";

/// The jobs a pipeline run must find in the workflow before it may start it: the plan, the
/// checks, the tag, the publish and the bump. (No site job: none exists.)
const CHAIN: &[&str] = &["plan", "verify", "tag", PUBLISH, "bump"];

/// The environment of the jobs that hold the release bot's key or write.
const ENVIRONMENT: &str = "release";

/// The commit the pipeline run built, and its id, as the workflow sets `COMMIT` and
/// `PIPELINE_RUN` (a `workflow_run` run's own `github.sha` is the latest commit of main).
pub const COMMIT_EXPR: &str = "${{ github.event.workflow_run.head_sha }}";
pub const RUN_EXPR: &str = "${{ github.event.workflow_run.id }}";

/// The `if:` of `verify`: a push that ships code and does not change the pipeline.
pub const VERIFY_IF: &str =
    "${{ needs.plan.outputs.code == 'true' && needs.plan.outputs.pipeline != 'true' }}";

/// The `if:` of every job that tags, publishes or bumps: a successful pipeline run for a push to
/// `main` (so a run that was cancelled, or red, writes nothing), of code, that is not a change
/// to the release pipeline itself.
pub const RELEASE_IF: &str = "${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository && needs.plan.outputs.code == 'true' && needs.plan.outputs.pipeline != 'true' }}";

/// The `if:` of `plan`, which every other job needs: the run that started this one succeeded (so
/// ci, artifacts and beta all did, each needing the one before), for a push, to main, of this
/// repository.
pub const PLAN_IF: &str = "${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository }}";

/// The jobs that may hold `id-token: write`: none (the kbf.dev upload is a later change that
/// adds its job here).
const ID_TOKEN: &[&str] = &[];

/// The `(job, scope)` pairs that may be `write`.
const WRITERS: &[(&str, &str)] = &[(PUBLISH, "contents")];

/// What a job must be.
struct Rule {
    job: &'static str,
    needs: &'static [&'static str],
    permissions: &'static [(&'static str, &'static str)],
    condition: Option<&'static str>,
    environment: Option<&'static str>,
}

const READ: &[(&str, &str)] = &[
    ("contents", "read"),
    ("actions", "read"),
    ("attestations", "read"),
];

const RULES: [Rule; 6] = [
    Rule {
        job: "plan",
        needs: &[],
        permissions: &[("contents", "read"), ("actions", "read")],
        condition: Some(PLAN_IF),
        environment: None,
    },
    Rule {
        job: "verify",
        needs: &["plan"],
        permissions: READ,
        condition: Some(VERIFY_IF),
        environment: None,
    },
    Rule {
        job: "tag",
        needs: &["plan", "verify"],
        permissions: &[("contents", "read")],
        condition: Some(RELEASE_IF),
        environment: Some(ENVIRONMENT),
    },
    Rule {
        job: "notes",
        needs: &["plan", "verify", "tag"],
        permissions: READ,
        condition: Some(RELEASE_IF),
        environment: None,
    },
    Rule {
        job: PUBLISH,
        needs: &["plan", "verify", "tag", "notes"],
        permissions: &[("contents", "write")],
        condition: Some(RELEASE_IF),
        environment: Some(ENVIRONMENT),
    },
    Rule {
        job: "bump",
        needs: &["plan", "tag", PUBLISH],
        permissions: &[("contents", "read")],
        condition: Some(RELEASE_IF),
        environment: Some(ENVIRONMENT),
    },
];

/// The jobs that read the release bot's key and mint its token.
const BOT_JOBS: &[&str] = &["tag", "bump"];
/// The action that mints it, pinned in `workflows` like every action.
const APP_TOKEN: &str = "actions/create-github-app-token@";
/// The jobs that write must (transitively) need these.
const GATING: &[&str] = &["plan", "verify"];
/// The jobs that write, or hold a token that can.
const WRITING: &[&str] = &["tag", "notes", PUBLISH, "bump"];

/// The group and the no-cancel of the one release at a time. A pipeline run that did not
/// succeed starts this workflow too (every job skips); its group is its own, so it never
/// takes the place of a release that is waiting in `release-main`.
const GROUP: &str =
    "release-${{ github.event.workflow_run.conclusion == 'success' && 'main' || github.run_id }}";

/// The expressions an input of a run could reach a script or a checkout through.
const UNTRUSTED: &[&str] = &[
    "inputs.",
    "github.event.inputs",
    "github.event.pull_request",
    "github.head_ref",
    "github.event.workflow_run.pull_requests",
    "github.event.workflow_run.head_commit",
    "github.event.workflow_run.display_title",
];
/// Parts of the run that started this one that a person chooses (a branch name), and that may
/// be read in an `if:` only.
const IF_ONLY: &[&str] = &["github.event.workflow_run.head_branch"];
/// Status functions and keys that let a job pass or run after a red one.
const CONTINUING: &[&str] = &["always()", "failure()", "cancelled()", "continue-on-error"];

/// The artifact the notes are uploaded as, and the one the verified files travel as.
const NOTES_ARTIFACT: &str = "release-notes";
const ASSETS_ARTIFACT: &str = "release-assets";
const NAME_KEY: &str = "name";
/// The directories of verified release files whose binaries `notes` may run.
const DIST: &str = "dist";
const PREVIOUS_DIR: &str = "previous";

fn lines(step: &workflows::Step) -> impl Iterator<Item = &str> {
    step.run
        .as_deref()
        .unwrap_or_default()
        .lines()
        .map(str::trim_start)
}

/// The words after `prefix` of the first line in `steps` that starts with it (double
/// quotes dropped), with the step's position.
fn command<'a>(
    steps: &'a [workflows::Step],
    prefix: &str,
) -> Option<(usize, &'a workflows::Step, Vec<&'a str>)> {
    steps.iter().enumerate().find_map(|(at, s)| {
        lines(s).find_map(|l| {
            l.strip_prefix(prefix).map(|rest| {
                let words = rest
                    .split_whitespace()
                    .map(|w| w.trim_matches('"'))
                    .collect();
                (at, s, words)
            })
        })
    })
}

fn value<'a>(words: &[&'a str], flag: &str) -> Option<&'a str> {
    let at = words.iter().position(|w| *w == flag)?;
    words.get(at + 1).copied()
}

fn uses(step: &workflows::Step, action: &str) -> bool {
    step.uses.as_deref().is_some_and(|u| u.starts_with(action))
}

fn input<'a>(step: &'a workflows::Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn env<'a>(step: &'a workflows::Step, key: &str) -> Option<&'a str> {
    step.env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The lines of the workflow that are not comments.
fn code(release: &str) -> String {
    release
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of the job `job` (its header line to the next job's), comments dropped.
fn job_text(release: &str, job: &str) -> String {
    let header = format!("  {job}:");
    let mut inside = false;
    let mut out = Vec::new();
    let text = code(release);
    for line in text.lines() {
        let top =
            line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':');
        if top {
            inside = line.trim_end() == header;
        }
        if inside {
            out.push(line);
        }
    }
    out.join("\n")
}

/// What differs between `release.yml` and the rules above: one line per difference,
/// empty when there is none. An error when the workflow cannot be read.
pub fn problems(release: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    triggers(release, &mut out)?;
    graph(release, &mut out)?;
    halting(release, &mut out)?;
    secrets(release, &mut out)?;
    checkouts(release, &mut out)?;
    context(release, &mut out)?;
    plan_job(release, &mut out)?;
    downloads_of_this_run(release, &mut out)?;
    verify_job(release, &mut out)?;
    tag_job(release, &mut out)?;
    notes_job(release, &mut out)?;
    publish_job(release, &mut out)?;
    bump_job(release, &mut out)?;
    // The shared rules, as the workflow lint reads them (pins, runners, permissions).
    out.extend(
        workflows::scan(release, &workflows::Repo::default())
            .into_iter()
            .map(|p| format!("workflow lint: {p}")),
    );
    Ok(out)
}

fn triggers(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let events = workflows::events(release)?;
    for event in &events {
        if !EVENTS.contains(&event.as_str()) {
            out.push(format!(
                "the workflow runs on `{event}`, not only {EVENTS:?}: a release starts from a successful pipeline run and from nothing else"
            ));
        }
    }
    for event in EVENTS {
        if !events.iter().any(|e| e == event) {
            out.push(format!("the workflow does not run on `{event}`"));
        }
    }
    if events.iter().any(|e| e == "workflow_run") {
        let jobs = workflows::job_names(release)?;
        for job in CHAIN {
            if !jobs.iter().any(|j| j == job) {
                out.push(format!(
                    "the workflow runs on `workflow_run` but has no job `{job}`: a pipeline run would cut a release without it"
                ));
            }
        }
        let mut filters = workflows::trigger_filters(release, "workflow_run").unwrap_or_default();
        filters.sort();
        let want = [
            ("branches".to_owned(), vec!["main".to_owned()]),
            ("types".to_owned(), vec![TYPES.to_owned()]),
            ("workflows".to_owned(), vec![PIPELINE.to_owned()]),
        ];
        if filters != want {
            out.push(format!(
                "`workflow_run` is filtered by {filters:?}, not exactly `workflows: [{PIPELINE}]`, `types: [{TYPES}]` and `branches: [main]`: another workflow, or a run for another branch, would cut a release"
            ));
        }
    }
    let names = workflows::job_names(release)?;
    if names != JOBS {
        out.push(format!(
            "the jobs are {names:?}, not {JOBS:?} in that order (`{PUBLISH}` by that exact name)"
        ));
    }
    let concurrency = workflows::concurrency(release)?;
    let want = Some((GROUP.to_owned(), "false".to_owned()));
    if concurrency != want {
        out.push(format!(
            "the concurrency is {concurrency:?}, not `group: {GROUP}` with `cancel-in-progress: false`: one release at a time, none cancelled, and a run that releases nothing in a group of its own"
        ));
    }
    Ok(())
}

fn sorted(items: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    let mut v: Vec<String> = items.into_iter().map(Into::into).collect();
    v.sort();
    v
}

/// The jobs `job` needs, directly or through others.
fn closure(release: &str, job: &str) -> Result<Vec<String>, String> {
    let mut seen: Vec<String> = Vec::new();
    let mut todo = vec![job.to_owned()];
    while let Some(next) = todo.pop() {
        for n in workflows::job_needs(release, &next)? {
            if !seen.contains(&n) {
                seen.push(n.clone());
                todo.push(n);
            }
        }
    }
    Ok(seen)
}

fn graph(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    for rule in &RULES {
        let job = rule.job;
        let needs = workflows::job_needs(release, job)?;
        if sorted(needs.clone()) != sorted(rule.needs.iter().copied()) {
            out.push(format!(
                "the job `{job}` needs {needs:?}, not {:?}",
                rule.needs
            ));
        }
        let have = workflows::job_permission_levels(release, job)?;
        let want: Vec<String> = rule
            .permissions
            .iter()
            .map(|(s, l)| format!("{s}: {l}"))
            .collect();
        let got: Vec<String> = have.iter().map(|(s, l)| format!("{s}: {l}")).collect();
        if sorted(got.clone()) != sorted(want.clone()) {
            out.push(format!(
                "the job `{job}` holds the scopes {got:?}, not exactly {want:?}"
            ));
        }
        for (scope, level) in &have {
            let on = level != "read" && level != "none";
            if on && !WRITERS.contains(&(job, scope.as_str())) && scope != "id-token" {
                out.push(format!(
                    "the job `{job}` holds `{scope}: {level}`: only {WRITERS:?} may write"
                ));
            }
            if scope == "id-token" && on && !ID_TOKEN.contains(&job) {
                out.push(format!(
                    "the job `{job}` holds `id-token: {level}`, which only {ID_TOKEN:?} may"
                ));
            }
        }
        let condition = workflows::job_condition(release, job)?;
        if condition.as_deref() != rule.condition {
            out.push(format!(
                "the job `{job}` has `if: {}`, not {}",
                condition.as_deref().unwrap_or("(none)"),
                rule.condition
                    .map_or("none".to_owned(), |c| format!("exactly `{c}`")),
            ));
        }
        let environment = workflows::job_environment(release, job)?;
        if environment.as_deref() != rule.environment {
            out.push(format!(
                "the job `{job}` has environment {environment:?}, not {:?}",
                rule.environment
            ));
        }
    }
    for job in WRITING {
        let needs = closure(release, job)?;
        for gate in GATING {
            if !needs.iter().any(|n| n == gate) {
                out.push(format!(
                    "the job `{job}` does not need `{gate}`, directly or through another job: it could run on a commit the pipeline did not pass"
                ));
            }
        }
    }
    Ok(())
}

/// No way for a job to pass or run after a red one, and no step under `if:` but the
/// previous release's in `notes`.
fn halting(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let code = code(release);
    for word in CONTINUING {
        if code.contains(word) {
            out.push(format!(
                "the workflow uses `{word}`: a red job would not stop the release"
            ));
        }
    }
    for job in JOBS {
        for step in workflows::job_steps(release, job)? {
            let previous = step
                .condition
                .as_deref()
                .is_some_and(|c| c == "${{ steps.previous.outputs.previous != '' }}");
            if step.conditional && !(job == "notes" && previous) {
                out.push(format!(
                    "a step of `{job}` ({:?}) is under `if:` or `continue-on-error:`: only the previous release's steps of `notes` may be",
                    step.name.as_deref().or(step.uses.as_deref()).unwrap_or("?")
                ));
            }
        }
    }
    Ok(())
}

/// Who holds the release bot's key, and nothing a person or a pull request typed.
fn secrets(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let all = code(release);
    for word in UNTRUSTED {
        if all.contains(word) {
            out.push(format!(
                "the workflow reads `{word}`: only the commit the pipeline run built is released, never what a person or a pull request names"
            ));
        }
    }
    for word in IF_ONLY {
        let elsewhere = all.lines().any(|l| {
            l.contains(word)
                && !(l.trim_start().starts_with("if: ${{ ") && l.trim_end().ends_with("}}"))
        });
        if elsewhere {
            out.push(format!(
                "the workflow reads `{word}` outside an `if:`: a branch name is chosen by a person and must not reach a script"
            ));
        }
    }
    if all.contains("RELEASE_TAGGER") {
        out.push(
            "the workflow names `RELEASE_TAGGER_*`: the one App is `RELEASE_BOT_*`".to_owned(),
        );
    }
    for job in JOBS {
        let text = job_text(release, job);
        let bot = BOT_JOBS.contains(&job);
        if text.contains("RELEASE_BOT_") && !bot {
            out.push(format!(
                "the job `{job}` reads `RELEASE_BOT_*`: only {BOT_JOBS:?} may hold the release bot's key"
            ));
        }
        if text.contains(APP_TOKEN) && !bot {
            out.push(format!(
                "the job `{job}` mints an App token: only {BOT_JOBS:?} may"
            ));
        }
        if bot {
            for var in ["vars.RELEASE_BOT_APP_ID", "secrets.RELEASE_BOT_PRIVATE_KEY"] {
                if !text.contains(var) {
                    out.push(format!("the job `{job}` does not read `{var}`"));
                }
            }
        }
        if text.contains("RELEASE_FREEZE") && job != "plan" {
            out.push(format!(
                "the job `{job}` reads `RELEASE_FREEZE`: the stop switch is read in `plan` alone"
            ));
        }
        for at in text.match_indices("secrets.").map(|(i, _)| &text[i..]) {
            if !at.starts_with("secrets.RELEASE_BOT_PRIVATE_KEY") {
                out.push(format!(
                    "the job `{job}` reads a secret other than `RELEASE_BOT_PRIVATE_KEY`"
                ));
            }
        }
    }
    if !job_text(release, "plan").contains("vars.RELEASE_FREEZE") {
        out.push(
            "the job `plan` does not read `vars.RELEASE_FREEZE`: nothing would stop the releases"
                .to_owned(),
        );
    }
    Ok(())
}

/// Every checkout is of the commit the pipeline run built, with no credentials left behind.
fn checkouts(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    for job in JOBS {
        let steps = workflows::job_steps(release, job)?;
        let mut seen = 0;
        for step in steps.iter().filter(|s| uses(s, "actions/checkout@")) {
            seen += 1;
            for (key, _) in &step.with {
                if !["fetch-depth", "persist-credentials", "ref"].contains(&key.as_str()) {
                    out.push(format!(
                        "the checkout of `{job}` is given `{key}`: only the commit the pipeline run built is checked out"
                    ));
                }
            }
            if input(step, "ref") != Some(COMMIT_EXPR) {
                out.push(format!(
                    "the checkout of `{job}` is not of `ref: {COMMIT_EXPR}`: a run started by `workflow_run` checks out the latest commit of main otherwise"
                ));
            }
            if input(step, "persist-credentials") != Some("false") {
                out.push(format!(
                    "the checkout of `{job}` is not `persist-credentials: false`: a later step would hold the token"
                ));
            }
        }
        if seen != 1 {
            out.push(format!("the job `{job}` has {seen} checkouts, not one"));
        }
    }
    Ok(())
}

fn full_history(steps: &[workflows::Step]) -> bool {
    steps
        .iter()
        .find(|s| uses(s, "actions/checkout@"))
        .is_some_and(|s| input(s, "fetch-depth") == Some("0"))
}

/// The output `name` of the job must be the step output `from` of the step `step`.
fn output_is(
    release: &str,
    job: &str,
    name: &str,
    step: &str,
    from: &str,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let want = format!("${{{{ steps.{step}.outputs.{from} }}}}");
    if !workflows::job_outputs(release, job)?
        .iter()
        .any(|(k, v)| k == name && *v == want)
    {
        out.push(format!(
            "the job `{job}` does not output `{name}` as `{want}`"
        ));
    }
    Ok(())
}

fn flag_is(words: &[&str], verb: &str, flag: &str, want: &str, out: &mut Vec<String>) {
    if value(words, flag) != Some(want) {
        out.push(format!("`{verb}` is not given `{flag} {want}`"));
    }
}

/// The step that runs `prefix`, unconditional, with its words; a problem when absent.
fn run_of<'a>(
    job: &str,
    steps: &'a [workflows::Step],
    prefix: &str,
    out: &mut Vec<String>,
) -> Option<(usize, &'a workflows::Step, Vec<&'a str>)> {
    let found = command(steps, prefix);
    if found.is_none() {
        out.push(format!(
            "the job `{job}` does not run `{}`",
            prefix.trim_end()
        ));
    }
    found
}

fn env_is(job: &str, step: &workflows::Step, key: &str, want: &str, out: &mut Vec<String>) {
    if env(step, key) != Some(want) {
        out.push(format!(
            "the step of `{job}` that runs `{}` does not set `{key}: {want}`",
            step.run
                .as_deref()
                .unwrap_or("?")
                .split_whitespace()
                .take(4)
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
}

/// The signer an attestation must come from: the workflow that builds the artifacts.
const SIGNER: &str = "$GITHUB_REPOSITORY/.github/workflows/artifacts.yml";

/// `job` verifies the attestations as a deployment does (this repository, the artifacts
/// workflow as signer, this commit) and checks the sums by `sha256sum` before it does.
fn files_are_checked(job: &str, steps: &[workflows::Step], out: &mut Vec<String>) {
    if let Some((_, _, words)) = command(steps, "cargo xtask verify-assets attestations ") {
        let verb = "cargo xtask verify-assets attestations";
        flag_is(&words, verb, "--repo", "$GITHUB_REPOSITORY", out);
        flag_is(&words, verb, "--workflow-ref", SIGNER, out);
        flag_is(&words, verb, "--commit", "$COMMIT", out);
    }
    let sums = steps.iter().any(|s| {
        !s.conditional
            && s.run
                .as_deref()
                .is_some_and(|r| r.trim() == "sha256sum --check --strict SHA256SUMS")
    });
    if !sums {
        out.push(format!(
            "the job `{job}` does not check the files against SHA256SUMS"
        ));
    }
}

/// The commit and the pipeline run are set once, for every job, from the run that started this one.
fn context(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let env = workflows::top_env(release)?;
    for (name, want) in [("COMMIT", COMMIT_EXPR), ("PIPELINE_RUN", RUN_EXPR)] {
        if !env.iter().any(|(k, v)| k == name && v == want) {
            out.push(format!(
                "the workflow does not set `{name}: {want}`: every step would act on another commit or run than the one released"
            ));
        }
    }
    for job in JOBS {
        let own = workflows::job_env(release, job)?;
        if own
            .iter()
            .any(|(k, _)| k == "COMMIT" || k == "PIPELINE_RUN")
        {
            out.push(format!(
                "the job `{job}` sets `COMMIT` or `PIPELINE_RUN` itself: they are the workflow's alone"
            ));
        }
    }
    Ok(())
}

/// Only `plan` (the commit the push replaced) and `verify` (the files) read the artifacts of
/// another run, and each of the pipeline run that started this one: every later job takes the
/// files `verify` checked.
fn downloads_of_this_run(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    for rule in &RULES {
        let steps = workflows::job_steps(release, rule.job)?;
        let reads_other_run = ["plan", "verify"].contains(&rule.job);
        for d in steps
            .iter()
            .filter(|s| uses(s, "actions/download-artifact@"))
        {
            let named = input(d, "run-id");
            if !reads_other_run && named.is_some() {
                out.push(format!("a download of `{}` names a `run-id`: only `plan` and `verify` read another run, the rest take what `verify` checked", rule.job));
            }
            if reads_other_run && named != Some(RUN_EXPR) {
                out.push(format!("a download of `{}` is not of run `{RUN_EXPR}`: the files could come from another run than the one that started this", rule.job));
            }
            if named.is_some() && input(d, "github-token") != Some("${{ github.token }}") {
                out.push(format!(
                    "a download of `{}` names a `run-id` without `github-token: ${{{{ github.token }}}}`",
                    rule.job
                ));
            }
        }
    }
    Ok(())
}

fn plan_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, "plan")?;
    if !full_history(&steps) {
        out.push("the job `plan` does not check out with `fetch-depth: 0`: it could not diff the push or see the tags".to_owned());
    }
    let before = steps.iter().any(|s| {
        uses(s, "actions/download-artifact@")
            && input(s, NAME_KEY) == Some("push-before")
            && input(s, "path") == Some("push")
    });
    if !before {
        out.push("the job `plan` does not download `push-before` into `push`: it could not tell what the push changed".to_owned());
    }
    let verb = "cargo xtask release plan";
    if let Some((_, step, words)) = run_of("plan", &steps, "cargo xtask release plan ", out) {
        flag_is(&words, verb, "--sha", "$COMMIT", out);
        flag_is(&words, verb, "--before-file", "push/before", out);
        flag_is(&words, verb, "--freeze", "$FREEZE", out);
        flag_is(&words, verb, "--output", "$GITHUB_OUTPUT", out);
        env_is("plan", step, "FREEZE", "${{ vars.RELEASE_FREEZE }}", out);
        let id = step.id.as_deref().unwrap_or("");
        for name in ["version", "tag", "code", "site", "pipeline"] {
            output_is(release, "plan", name, id, name, out)?;
        }
    }
    Ok(())
}

fn verify_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let job = "verify";
    let steps = workflows::job_steps(release, job)?;
    if let Some((_, locate, words)) = run_of(job, &steps, "cargo xtask release locate ", out) {
        let verb = "cargo xtask release locate";
        flag_is(&words, verb, "--commit", "$COMMIT", out);
        flag_is(&words, verb, "--run-id", "$PIPELINE_RUN", out);
        if locate.conditional {
            out.push("the `release locate` step of `verify` is under `if:`".to_owned());
        }
    }
    let downloads: Vec<_> = steps
        .iter()
        .filter(|s| uses(s, "actions/download-artifact@"))
        .collect();
    if downloads.is_empty() {
        out.push("the job `verify` downloads no artifact of the pipeline run".to_owned());
    }
    for d in downloads {
        if input(d, "run-id") != Some(RUN_EXPR) {
            out.push(format!("a download of `verify` is not of run `{RUN_EXPR}`: the files could come from another run than the one that started this"));
        }
    }
    let checked = command(&steps, "cargo xtask release check ").map(|(at, ..)| at);
    if let Some((_, _, words)) = command(&steps, "cargo xtask release check ") {
        flag_is(
            &words,
            "cargo xtask release check",
            "--commit",
            "$COMMIT",
            out,
        );
    } else {
        out.push("the job `verify` does not run `cargo xtask release check`".to_owned());
    }
    let attested = command(&steps, "cargo xtask verify-assets attestations ").map(|(at, ..)| at);
    if attested.is_none_or(|a| checked.is_none_or(|c| a <= c)) {
        out.push(
            "the job `verify` does not verify the attestations after `release check`".to_owned(),
        );
    }
    files_are_checked(job, &steps, out);
    let upload = steps.iter().position(|s| {
        uses(s, "actions/upload-artifact@")
            && input(s, NAME_KEY) == Some(ASSETS_ARTIFACT)
            && input(s, "path") == Some("dist/")
            && input(s, "if-no-files-found") == Some("error")
    });
    if upload.is_none_or(|u| attested.is_none_or(|a| u <= a)) {
        out.push(format!("the job `verify` does not upload `dist/` as `{ASSETS_ARTIFACT}` after the attestations are verified: `tag` and `publish` would use files no one verified"));
    }
    Ok(())
}

/// A step that holds the token of the step `mint`: its `env` or `with` names its output.
fn holds_token(step: &workflows::Step, mint: &str) -> bool {
    let needle = format!("steps.{mint}.outputs.token");
    step.env
        .iter()
        .chain(&step.with)
        .any(|(_, v)| v.contains(&needle))
        || step.run.as_deref().is_some_and(|r| r.contains(&needle))
}

/// The step that mints the release bot's token, with `want` the exact inputs, in `steps`.
fn mint<'a>(
    job: &str,
    steps: &'a [workflows::Step],
    want: &[(&str, &str)],
    out: &mut Vec<String>,
) -> Option<(usize, &'a workflows::Step)> {
    let mints: Vec<_> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| uses(s, APP_TOKEN))
        .collect();
    if mints.len() != 1 {
        out.push(format!(
            "the job `{job}` mints {} App tokens, not one",
            mints.len()
        ));
        return None;
    }
    let (at, step) = mints[0];
    let mut have: Vec<String> = step.with.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut wanted: Vec<String> = want.iter().map(|(k, v)| format!("{k}={v}")).collect();
    have.sort();
    wanted.sort();
    if have != wanted {
        out.push(format!(
            "the token of `{job}` is minted with {have:?}, not exactly {wanted:?}: it must be scoped to the repository and permissions the job needs"
        ));
    }
    // Built before the token, so no build script runs beside it; and only the step that
    // writes holds it.
    for (i, s) in steps.iter().enumerate() {
        let id = step.id.as_deref().unwrap_or("");
        if i > at
            && s.run
                .as_deref()
                .is_some_and(|r| r.contains("cargo build") || r.contains("rustup"))
        {
            out.push(format!("the job `{job}` builds after the token is minted: a build script would run beside it"));
        }
        if i != at && holds_token(s, id) && i + 1 != steps.len() {
            out.push(format!(
                "a step of `{job}` that is not the last holds the bot's token"
            ));
        }
    }
    Some((at, step))
}

fn tag_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let job = "tag";
    let steps = workflows::job_steps(release, job)?;
    let want = [
        ("app-id", "${{ vars.RELEASE_BOT_APP_ID }}"),
        ("private-key", "${{ secrets.RELEASE_BOT_PRIVATE_KEY }}"),
        ("repositories", "${{ github.event.repository.name }}"),
        ("permission-contents", "write"),
    ];
    let minted = mint(job, &steps, &want, out);
    let verdict = run_of(job, &steps, "cargo xtask release verdict-write ", out);
    let tagged = run_of(job, &steps, "cargo xtask release tag ", out);
    let downloaded = steps.iter().position(|s| {
        uses(s, "actions/download-artifact@")
            && input(s, NAME_KEY) == Some(ASSETS_ARTIFACT)
            && input(s, "path") == Some(DIST)
    });
    let (Some((mint_at, bot)), Some((v_at, vs, vw)), Some((t_at, ts, tw))) =
        (minted, verdict, tagged)
    else {
        return Ok(());
    };
    if downloaded.is_none_or(|d| d >= v_at) {
        out.push(format!("the job `tag` does not download `{ASSETS_ARTIFACT}` into `{DIST}` before it writes the record: the record would name no set"));
    }
    if v_at >= mint_at || mint_at >= t_at {
        out.push(
            "the job `tag` does not write the record, then mint the token, then tag, in that order"
                .to_owned(),
        );
    }
    let verb = "cargo xtask release verdict-write";
    if vw.first() != Some(&DIST) {
        out.push(format!(
            "`{verb}` is not given `{DIST}`, the verified files"
        ));
    }
    flag_is(&vw, verb, "--commit", "$COMMIT", out);
    flag_is(&vw, verb, "--tag", "$TAG", out);
    flag_is(&vw, verb, "--pipeline-run", "$PIPELINE_RUN", out);
    env_is(job, vs, "TAG", "${{ needs.plan.outputs.tag }}", out);
    let record = value(&vw, "--out").unwrap_or("");
    let verb = "cargo xtask release tag";
    flag_is(&tw, verb, "--sha", "$COMMIT", out);
    flag_is(&tw, verb, "--tag", "$TAG", out);
    if record.is_empty() || value(&tw, "--message-file") != Some(record) {
        out.push(format!(
            "`{verb}` is not given `--message-file {record}`, the record `verdict-write` wrote"
        ));
    }
    env_is(job, ts, "TAG", "${{ needs.plan.outputs.tag }}", out);
    let want = format!(
        "${{{{ steps.{}.outputs.token }}}}",
        bot.id.as_deref().unwrap_or("")
    );
    env_is(job, ts, "GH_TOKEN", &want, out);
    Ok(())
}

fn bump_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let job = "bump";
    let steps = workflows::job_steps(release, job)?;
    let want = [
        ("app-id", "${{ vars.RELEASE_BOT_APP_ID }}"),
        ("private-key", "${{ secrets.RELEASE_BOT_PRIVATE_KEY }}"),
        ("owner", "komira-ai"),
        ("repositories", "komira-internal"),
        ("permission-contents", "write"),
        ("permission-pull-requests", "write"),
    ];
    let minted = mint(job, &steps, &want, out);
    let bumped = run_of(job, &steps, "cargo xtask release bump-internal ", out);
    let (Some((mint_at, bot)), Some((b_at, bs, bw))) = (minted, bumped) else {
        return Ok(());
    };
    if mint_at >= b_at {
        out.push("the job `bump` runs `bump-internal` before the token is minted".to_owned());
    }
    let verb = "cargo xtask release bump-internal";
    flag_is(&bw, verb, "--repo", "$GITHUB_REPOSITORY", out);
    flag_is(&bw, verb, "--version", "$VERSION", out);
    flag_is(&bw, verb, "--sha", "$COMMIT", out);
    flag_is(
        &bw,
        verb,
        "--internal-repo",
        "komira-ai/komira-internal",
        out,
    );
    flag_is(&bw, verb, "--submodule", "kbf", out);
    env_is(job, bs, "VERSION", "${{ needs.plan.outputs.version }}", out);
    let want = format!(
        "${{{{ steps.{}.outputs.token }}}}",
        bot.id.as_deref().unwrap_or("")
    );
    env_is(job, bs, "GH_TOKEN", &want, out);
    Ok(())
}

/// The file `notes` writes the notes to (its `release notes --out`).
fn notes_out(release: &str) -> Result<String, String> {
    let steps = workflows::job_steps(release, "notes")?;
    let words = command(&steps, "cargo xtask release notes ").map(|(.., w)| w);
    Ok(words
        .and_then(|w| value(&w, "--out").map(str::to_owned))
        .unwrap_or_default())
}

fn publish_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let job = PUBLISH;
    let steps = workflows::job_steps(release, job)?;
    let Some((at, step, words)) = run_of(job, &steps, "cargo xtask release publish ", out) else {
        return Ok(());
    };
    let verb = "cargo xtask release publish";
    if words.first() != Some(&DIST) {
        out.push(format!("`{verb}` is not given `{DIST}`"));
    }
    flag_is(&words, verb, "--tag", "$TAG", out);
    flag_is(&words, verb, "--commit", "$COMMIT", out);
    flag_is(&words, verb, "--run-id", "$RUN_ID", out);
    flag_is(&words, verb, "--set-hash", "$SET_HASH", out);
    env_is(job, step, "TAG", "${{ needs.plan.outputs.tag }}", out);
    env_is(job, step, "RUN_ID", RUN_EXPR, out);
    env_is(
        job,
        step,
        "SET_HASH",
        "${{ needs.notes.outputs.set-hash }}",
        out,
    );
    env_is(job, step, "GH_TOKEN", "${{ github.token }}", out);
    let notes = notes_out(release)?;
    if notes.is_empty() || value(&words, "--notes-file") != Some(notes.as_str()) {
        out.push(format!("`release publish` is not given `--notes-file {notes}`, the file `notes` wrote the notes to: the release body would not be the notes"));
    }
    let downloaded = |artifact: &str, path: &str| {
        steps.iter().take(at).any(|s| {
            uses(s, "actions/download-artifact@")
                && input(s, NAME_KEY) == Some(artifact)
                && input(s, "path").unwrap_or(path) == path
                && !s.conditional
        })
    };
    if !downloaded(NOTES_ARTIFACT, ".") {
        out.push(format!("the job `{job}` does not download the artifact `{NOTES_ARTIFACT}` (into the working directory), in a step before `release publish`: there would be no notes to publish"));
    }
    if !downloaded(ASSETS_ARTIFACT, DIST) {
        out.push(format!("the job `{job}` does not download the artifact `{ASSETS_ARTIFACT}` into `{DIST}` before `release publish`"));
    }
    let sums = steps.iter().take(at).any(|s| {
        s.run
            .as_deref()
            .is_some_and(|r| r.trim() == "sha256sum --check --strict SHA256SUMS")
    });
    if !sums {
        out.push(format!(
            "the job `{job}` does not check the files against SHA256SUMS before `release publish`"
        ));
    }
    Ok(())
}

fn notes_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let job = "notes";
    let steps = workflows::job_steps(release, job)?;
    if !full_history(&steps) {
        out.push("the job `notes` does not check out with `fetch-depth: 0`, so the annotated tag object and `origin/main` are not there".to_owned());
    }
    let env = workflows::job_env(release, job)?;
    for (name, wanted) in [("TAG", "${{ needs.plan.outputs.tag }}")] {
        if !env.iter().any(|(k, v)| k == name && v == wanted) {
            out.push(format!("the job `notes` does not set `{name}: {wanted}`: the tag and commit every step checks would not be the ones released"));
        }
    }
    let downloaded = steps.iter().position(|s| {
        uses(s, "actions/download-artifact@")
            && input(s, NAME_KEY) == Some(ASSETS_ARTIFACT)
            && input(s, "path") == Some(DIST)
    });
    let checked = command(&steps, "cargo xtask release check ").map(|(at, ..)| at);
    if checked.is_none_or(|c| downloaded.is_none_or(|d| c <= d)) {
        out.push(format!(
            "the job `notes` does not run `release check` after it downloads `{ASSETS_ARTIFACT}`"
        ));
    }
    match command(&steps, "cargo xtask release verify ") {
        None => out.push("the job `notes` does not run `cargo xtask release verify`: a tag naming another commit, or one not on main, would release".to_owned()),
        Some((at, step, words)) => {
            if checked.is_none_or(|c| at <= c) {
                out.push("`release verify` does not come after `release check`, so it would name a set not yet verified".to_owned());
            }
            for (flag, var) in [("--tag", "$TAG"), ("--commit", "$COMMIT"), ("--output", "$GITHUB_OUTPUT")] {
                flag_is(&words, "release verify", flag, var, out);
            }
            if value(&words, "--main-ref").is_some_and(|r| r != "origin/main") {
                out.push("`release verify` checks against a main ref other than `origin/main`".to_owned());
            }
            output_is(release, job, "set-hash", step.id.as_deref().unwrap_or(""), "set-hash", out)?;
        }
    }
    if let Some((at, _, words)) = run_of(job, &steps, "cargo xtask release notes ", out) {
        for (flag, var) in [("--tag", "$TAG"), ("--commit", "$COMMIT")] {
            flag_is(&words, "release notes", flag, var, out);
        }
        if value(&words, "--version-json").is_none() {
            out.push("`release notes` is not given `--version-json`: what is not reversible would not be computed from the build".to_owned());
        }
        flag_is(
            &words,
            "release notes",
            "--previous-version-json",
            "$PREVIOUS_IDENTITY",
            out,
        );
        let file = value(&words, "--out").unwrap_or("");
        let uploaded = steps.iter().skip(at + 1).any(|s| {
            uses(s, "actions/upload-artifact@")
                && input(s, NAME_KEY) == Some(NOTES_ARTIFACT)
                && !file.is_empty()
                && input(s, "path") == Some(file)
                && input(s, "if-no-files-found") == Some("error")
                && !s.conditional
        });
        if !uploaded {
            out.push(format!("the notes written to `{file}` are not uploaded, in a step after, as the artifact `{NOTES_ARTIFACT}`"));
        }
    }
    files_are_checked(job, &steps, out);
    identities(&steps, out);
    version_facts(&steps, out);
    Ok(())
}

/// The flag that holds a `release check` to no version facts of its `kbf` tarballs.
const WITHOUT_FACTS: &str = "--without-version-facts";

/// The release being made is checked with the version facts of its `kbf` (`release check
/// dist` has no waiver), and the previous release, made before the build jobs recorded
/// them, is checked without (`release check previous` has it): else the first release
/// with facts would fail on the one before it, or a set without them would go out.
fn version_facts(steps: &[workflows::Step], out: &mut Vec<String>) {
    for (dir, waived) in [(DIST, false), (PREVIOUS_DIR, true)] {
        let prefix = format!("cargo xtask release check {dir} ");
        let Some((_, _, words)) = command(steps, &prefix) else {
            continue;
        };
        if waived && !words.contains(&WITHOUT_FACTS) {
            out.push(format!("`release check {dir}` is not given `{WITHOUT_FACTS}`: the release before the version facts were recorded has none, so the first release with them would fail on it"));
        }
        if !waived && words.contains(&WITHOUT_FACTS) {
            out.push(format!("`release check {dir}` is given `{WITHOUT_FACTS}`: the release being made must hold the version facts of its kbf"));
        }
    }
}

/// A binary runs in `notes` only to print `--version --json`, and only after the
/// attestations of the directory it was unpacked from are verified, under the same `if:`.
fn identities(steps: &[workflows::Step], out: &mut Vec<String>) {
    let mut ran = 0;
    for (at, step) in steps.iter().enumerate() {
        let run = step.run.as_deref().unwrap_or_default();
        if !run.contains("--version --json") && !run.starts_with("cargo xtask release identity ") {
            continue;
        }
        ran += 1;
        let words = command(&steps[at..=at], "cargo xtask release identity ");
        let dir = words.as_ref().and_then(|(.., w)| w.first().copied());
        let Some(dir) = dir.filter(|d| [DIST, PREVIOUS_DIR].contains(d)) else {
            out.push(format!("step {at} of `notes` runs a binary that is not `release identity` on `{DIST}` or `{PREVIOUS_DIR}`: an unattested binary would run"));
            continue;
        };
        let prefix = format!("cargo xtask verify-assets attestations {dir} ");
        match command(&steps[..at], &prefix) {
            Some((_, check, _)) if check.condition == step.condition => {}
            _ => out.push(format!("step {at} of `notes` runs a binary unpacked from `{dir}/` without an earlier, equally conditioned `{prefix}...` step: an unattested binary would run")),
        }
    }
    if ran == 0 {
        out.push("the job `notes` never runs `release identity`: what is not reversible would not be computed from the builds".to_owned());
    }
}

#[cfg(test)]
mod tests;
