//! What `.github/workflows/release.yml` must keep: the automatic release of
//! docs/design/kbf-cicd.md (section 8.1) as workflow rules. [`problems`] reads the workflow and
//! reports each way it differs; the real file must report none, and `tests` plants a defect for
//! each rule on it.
//!
//! - **Events.** Exactly [`EVENTS`], and the dispatch takes no input. Today that is
//!   `workflow_dispatch` alone. The change that makes a push to `main` cut a release adds
//!   `push: branches: [main]` and `push` here, and nothing else (it lands last: every job and
//!   rule below exists before the trigger does).
//! - **Names gamma.yml depends on.** The workflow is named [`NAME`] (`gamma.yml` follows
//!   `workflows: [release]`), and its job `publish` ([`PUBLISH`]) exists; `gamma evidence`
//!   reads that job's conclusion. Renaming either stops every deploy to gamma silently.
//! - **The graph.** The jobs [`JOBS`], in that order, each with exactly the `needs`, token
//!   scopes, `if:` and `environment` of [`RULES`]; the jobs that write need `gates`, `plan` and
//!   `verify`, directly or through another. Only `publish` holds a write scope (`contents`),
//!   `id-token` is in no job ([`ID_TOKEN`]), and the jobs that tag, publish or bump run only
//!   under [`RELEASE_IF`]: a push to `main` of code that is not a change to the pipeline.
//! - **Halt on red.** No `always()`, `failure()`, `cancelled()` or `continue-on-error`, and no
//!   step under `if:` but the ones of `notes` that read the previous release.
//! - **One commit.** Every checkout is of `github.sha` (no `ref`, `repository` or `token`), and
//!   no expression reads `inputs.`, a pull request or `head_ref`.
//! - **Who holds the bot's key.** `RELEASE_BOT_*` and `actions/create-github-app-token` appear
//!   in `tag` and `bump` only, and both declare the `release` environment (so moving the key to
//!   an environment secret is a setting). `RELEASE_FREEZE` is read in `plan` only, and
//!   `RELEASE_TAGGER_*` nowhere. The token is minted after the build, scoped to one repository and
//!   the permissions the job needs, and only the step that writes holds it.
//! - **Data flow.** `gates` outputs the three run ids, `plan` the version, tag and class of
//!   the push, `verify` downloads the files of the run `gates` saw; `tag` writes the
//!   record from the gates and tags the planned tag at `github.sha` with that record; `notes`
//!   checks the tag and writes the notes and the set hash; `publish` is given those, the planned
//!   tag and `github.sha`; `bump` is given the planned version. Each flag below is held to the
//!   value that makes it mean what its verb says.
//! - **The shared rules.** Actions pinned to a full commit SHA, hosted runners and an empty
//!   top-level `permissions` are the workflow lint's (`workflows`); they are repeated here so a
//!   defect planted in this file reads red from this test too.

use super::workflows;

/// The workflow's name: `gamma.yml` follows it (`workflows: [release]`).
const NAME: &str = "release";
/// The job whose conclusion `gamma evidence` reads. Keep it and give it no `name:`.
const PUBLISH: &str = "publish";

/// The jobs, in file order.
const JOBS: [&str; 7] = ["gates", "plan", "verify", "tag", "notes", PUBLISH, "bump"];

/// The events that start the workflow. Add `push` here with the `push: branches: [main]` that
/// starts it, in the change that does only that.
const EVENTS: &[&str] = &["workflow_dispatch"];

/// The environment of the jobs that hold the release bot's key or write.
const ENVIRONMENT: &str = "release";

/// The `if:` of `verify`: a push that ships code and does not change the pipeline.
pub const VERIFY_IF: &str =
    "${{ needs.plan.outputs.code == 'true' && needs.plan.outputs.pipeline != 'true' }}";

/// The `if:` of every job that tags, publishes or bumps: a push to `main` (never a dispatch,
/// so a dry run writes nothing), of code, that is not a change to the release pipeline itself.
pub const RELEASE_IF: &str = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.code == 'true' && needs.plan.outputs.pipeline != 'true' }}";

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

const RULES: [Rule; 7] = [
    Rule {
        job: "gates",
        needs: &[],
        permissions: &[("contents", "read"), ("actions", "read")],
        condition: None,
        environment: None,
    },
    Rule {
        job: "plan",
        needs: &["gates"],
        permissions: &[("contents", "read")],
        condition: None,
        environment: None,
    },
    Rule {
        job: "verify",
        needs: &["gates", "plan"],
        permissions: READ,
        condition: Some(VERIFY_IF),
        environment: None,
    },
    Rule {
        job: "tag",
        needs: &["gates", "plan", "verify"],
        permissions: &[("contents", "read")],
        condition: Some(RELEASE_IF),
        environment: Some(ENVIRONMENT),
    },
    Rule {
        job: "notes",
        needs: &["gates", "plan", "verify", "tag"],
        permissions: READ,
        condition: Some(RELEASE_IF),
        environment: None,
    },
    Rule {
        job: PUBLISH,
        needs: &["gates", "plan", "verify", "tag", "notes"],
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
const GATING: &[&str] = &["gates", "plan", "verify"];
/// The jobs that write, or hold a token that can.
const WRITING: &[&str] = &["tag", "notes", PUBLISH, "bump"];

/// The group and the no-cancel of the one release at a time.
const GROUP: &str = "release-${{ github.event_name == 'push' && 'push' || github.run_id }}";

/// The expressions an input of a run could reach a script or a checkout through.
const UNTRUSTED: &[&str] = &[
    "inputs.",
    "github.event.inputs",
    "github.event.pull_request",
    "github.head_ref",
];
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
    gates_job(release, &mut out)?;
    plan_job(release, &mut out)?;
    downloads_of_this_run(release, &mut out)?;
    gates_outlast_wait(release, &mut out);
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
    if workflows::workflow_name(release)?.as_deref() != Some(NAME) {
        out.push(format!(
            "the workflow is not named `{NAME}`: `gamma.yml` follows `workflows: [{NAME}]`, so no deploy to gamma would follow a release"
        ));
    }
    let events = workflows::events(release)?;
    for event in &events {
        if !EVENTS.contains(&event.as_str()) {
            out.push(format!(
                "the workflow runs on `{event}`, not only {EVENTS:?}: the push trigger is added alone, last"
            ));
        }
    }
    for event in EVENTS {
        if !events.iter().any(|e| e == event) {
            out.push(format!("the workflow does not run on `{event}`"));
        }
    }
    if events.iter().any(|e| e == "push") {
        let filters = workflows::push_filters(release).unwrap_or_default();
        if filters != [("branches".to_owned(), vec!["main".to_owned()])] {
            out.push(format!(
                "`push` is filtered by {filters:?}, not exactly `branches: [main]`: a push of another branch or a tag would cut a release"
            ));
        }
    }
    if !workflows::dispatch_inputs(release)?.is_empty() {
        out.push("the `workflow_dispatch` trigger declares inputs: a run releases `github.sha` and nothing a person types".to_owned());
    }
    let names = workflows::job_names(release)?;
    if names != JOBS {
        out.push(format!(
            "the jobs are {names:?}, not {JOBS:?} in that order (`{PUBLISH}` by that exact name: `gamma.yml` reads its conclusion)"
        ));
    }
    let code = code(release);
    let group = code.lines().find_map(|l| l.strip_prefix("  group: "));
    if group != Some(GROUP) {
        out.push(format!(
            "the concurrency group is {group:?}, not `{GROUP}`: one release at a time, a dispatch in a group of its own"
        ));
    }
    if !code.lines().any(|l| l == "  cancel-in-progress: false") {
        out.push("the concurrency does not set `cancel-in-progress: false`: a release in flight would be cancelled".to_owned());
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
                    "the job `{job}` does not need `{gate}`, directly or through another job: it could run on a commit the gates did not pass"
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
                "the workflow reads `{word}`: only `github.sha` of a run is released, never what a person or a pull request names"
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

/// Every checkout is of `github.sha`, with no credentials left behind.
fn checkouts(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    for job in JOBS {
        let steps = workflows::job_steps(release, job)?;
        let mut seen = 0;
        for step in steps.iter().filter(|s| uses(s, "actions/checkout@")) {
            seen += 1;
            for (key, _) in &step.with {
                if key != "fetch-depth" && key != "persist-credentials" {
                    out.push(format!(
                        "the checkout of `{job}` is given `{key}`: only `github.sha` is checked out"
                    ));
                }
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

/// Only `verify` reads a run other than this one's: every later job takes the files `verify`
/// uploaded, which it checked.
fn downloads_of_this_run(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    for rule in &RULES {
        if rule.job == "verify" {
            continue;
        }
        let steps = workflows::job_steps(release, rule.job)?;
        if steps
            .iter()
            .any(|s| uses(s, "actions/download-artifact@") && input(s, "run-id").is_some())
        {
            out.push(format!("a download of `{}` names a `run-id`: only `verify` reads another run, the rest take what it verified", rule.job));
        }
    }
    Ok(())
}

/// A job that waits for the gates outlasts the wait: `gates` gives up after this many
/// minutes (`release gates --timeout-s` is 3 hours), so its own timeout is longer.
const GATES_GIVE_UP_MINUTES: u32 = 180;

fn gates_outlast_wait(release: &str, out: &mut Vec<String>) {
    let minutes = job_text(release, "gates").lines().find_map(|l| {
        l.trim()
            .strip_prefix("timeout-minutes:")?
            .trim()
            .parse::<u32>()
            .ok()
    });
    if minutes.is_none_or(|m| m <= GATES_GIVE_UP_MINUTES) {
        out.push(format!("the job `gates` has no `timeout-minutes` above {GATES_GIVE_UP_MINUTES}: the job would be killed before the wait for the gates gives up"));
    }
}

fn gates_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, "gates")?;
    let verb = "cargo xtask release gates";
    if let Some((_, step, words)) = run_of("gates", &steps, "cargo xtask release gates ", out) {
        flag_is(&words, verb, "--sha", "$GITHUB_SHA", out);
        flag_is(&words, verb, "--branch", "main", out);
        flag_is(&words, verb, "--repo", "$GITHUB_REPOSITORY", out);
        flag_is(&words, verb, "--output", "$GITHUB_OUTPUT", out);
        let id = step.id.as_deref().unwrap_or("");
        for (name, from) in [
            ("ci-run", "ci_run"),
            ("beta-run", "beta_run"),
            ("artifacts-run", "artifacts_run"),
        ] {
            output_is(release, "gates", name, id, from, out)?;
        }
    }
    Ok(())
}

/// The expression for `--before`: a dispatch has none, and all zeros reads as a first push.
pub const BEFORE_EXPR: &str =
    "${{ github.event.before || '0000000000000000000000000000000000000000' }}";

fn plan_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, "plan")?;
    if !full_history(&steps) {
        out.push("the job `plan` does not check out with `fetch-depth: 0`: it could not diff the push or see the tags".to_owned());
    }
    let verb = "cargo xtask release plan";
    if let Some((_, step, words)) = run_of("plan", &steps, "cargo xtask release plan ", out) {
        flag_is(&words, verb, "--sha", "$GITHUB_SHA", out);
        flag_is(&words, verb, "--before", "$BEFORE", out);
        flag_is(&words, verb, "--freeze", "$FREEZE", out);
        flag_is(&words, verb, "--output", "$GITHUB_OUTPUT", out);
        env_is("plan", step, "BEFORE", BEFORE_EXPR, out);
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
    let envs = workflows::job_env(release, job)?;
    if !envs
        .iter()
        .any(|(k, v)| k == "COMMIT" && v == "${{ github.sha }}")
    {
        out.push("the job `verify` does not set `COMMIT: ${{ github.sha }}`: every step would check another commit than the one released".to_owned());
    }
    if let Some((_, locate, words)) = run_of(job, &steps, "cargo xtask release locate ", out) {
        flag_is(
            &words,
            "cargo xtask release locate",
            "--commit",
            "$COMMIT",
            out,
        );
        if locate.conditional {
            out.push("the `release locate` step of `verify` is under `if:`".to_owned());
        }
    }
    let gated = "${{ needs.gates.outputs.artifacts-run }}";
    let downloads: Vec<_> = steps
        .iter()
        .filter(|s| uses(s, "actions/download-artifact@"))
        .collect();
    if downloads.is_empty() {
        out.push("the job `verify` downloads no artifact of the artifacts run".to_owned());
    }
    for d in downloads {
        if input(d, "run-id") != Some(gated) {
            out.push(format!("a download of `verify` is not of run `{gated}`: the files could come from another run than the one the gates saw"));
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
    flag_is(&vw, verb, "--commit", "$GITHUB_SHA", out);
    flag_is(&vw, verb, "--tag", "$TAG", out);
    flag_is(&vw, verb, "--ci-run", "$CI_RUN", out);
    flag_is(&vw, verb, "--beta-run", "$BETA_RUN", out);
    flag_is(&vw, verb, "--artifacts-run", "$ARTIFACTS_RUN", out);
    env_is(job, vs, "TAG", "${{ needs.plan.outputs.tag }}", out);
    env_is(job, vs, "CI_RUN", "${{ needs.gates.outputs.ci-run }}", out);
    env_is(
        job,
        vs,
        "BETA_RUN",
        "${{ needs.gates.outputs.beta-run }}",
        out,
    );
    env_is(
        job,
        vs,
        "ARTIFACTS_RUN",
        "${{ needs.gates.outputs.artifacts-run }}",
        out,
    );
    let record = value(&vw, "--out").unwrap_or("");
    let verb = "cargo xtask release tag";
    flag_is(&tw, verb, "--sha", "$GITHUB_SHA", out);
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
    flag_is(&bw, verb, "--sha", "$GITHUB_SHA", out);
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
    flag_is(&words, verb, "--commit", "$GITHUB_SHA", out);
    flag_is(&words, verb, "--run-id", "$RUN_ID", out);
    flag_is(&words, verb, "--set-hash", "$SET_HASH", out);
    env_is(job, step, "TAG", "${{ needs.plan.outputs.tag }}", out);
    env_is(
        job,
        step,
        "RUN_ID",
        "${{ needs.gates.outputs.artifacts-run }}",
        out,
    );
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
    for (name, wanted) in [
        ("COMMIT", "${{ github.sha }}"),
        ("TAG", "${{ needs.plan.outputs.tag }}"),
    ] {
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
