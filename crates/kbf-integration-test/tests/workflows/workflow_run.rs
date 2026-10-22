//! The one `workflow_run` the workflow lint allows.
//!
//! A `workflow_run` workflow starts when another workflow completes, runs the default
//! branch's copy of the file whatever ref the other run was for, and holds this repository's
//! token and its environments' credentials. So only `gamma.yml`, the workflow named
//! `gamma`, may have one, and [`rules`] holds that file to its exact shape (every message
//! begins `line 0:`: the readers it uses give no line):
//! - the trigger is `workflow_run` alone (no dispatch, schedule, pull request or push) of
//!   the workflow `release`, type `completed`, branch `main`;
//! - the jobs are `offer`, `deploy` and `check`, each with the exact `if:` of [`OFFER_IF`] and
//!   [`DEPLOY_IF`] (a successful push run to `main` of this repository; `deploy` and `check`
//!   also need the staged offer and the three repository variables, so an unset one is a
//!   green skip of both);
//! - the permissions are exactly the read scopes of [`PERMISSIONS`] (`contents: read` for
//!   `check`), and no `id-token` anywhere; `offer` has no environment, and `deploy` and
//!   `check` have `gamma`; `deploy` needs `offer` and `check` needs `offer` and `deploy`;
//! - the word `secrets` appears only in the `with:` of the Tailscale step of `deploy` and of
//!   `check`, each after the step that builds `cargo xtask`, and in the `env:` of the one
//!   `check-build` step (the farm's buckconfig); no other job joins the tailnet;
//! - the host and the user (`GAMMA_HOST`, `GAMMA_USER`) are named by the step that runs
//!   `gamma deploy` alone: GitHub prints a variable in the log of a step that has it;
//! - one deploy at a time is the `concurrency` of the `deploy` and `check` jobs (so a check
//!   never runs during a roll), none of `offer` or of the workflow (a pending run of the
//!   workflow would be cancelled by a newer run whose deploy is skipped);
//! - komira's text is checked out once, by `check` only, after the build of `cargo xtask`,
//!   as exactly [`KOMIRA`], and runs only inside the one step that runs `cargo xtask gamma
//!   check-build`, which has exactly the two variables of [`CHECK_ENV`]; the job runs no other
//!   script than the toolchain, the build and that step; `no-remote-cache` appears nowhere
//!   in the file (the check is of the farm with its cache);
//! - any other checkout names no `ref` and no `repository`, no `run:` script holds an event
//!   field (it goes through `env:`), and the artifacts uploaded are exactly those of
//!   [`UPLOADS`]: the offer's tar, and the redacted summary, never the installer's own; each
//!   upload overwrites, so a re-run of its job does not fail on the name.

use super::*;

/// The name of the only workflow that may trigger on `workflow_run`.
const NAME: &str = "gamma";

/// The workflow it follows.
const SOURCE: &str = "release";

/// What a workflow that is not `gamma` is told.
const NOT_GAMMA: &str = "triggers on workflow_run, which only the workflow named `gamma` may";

/// The `if:` of the `offer` job, exactly: the run it follows succeeded, was a push to `main`
/// and is of this repository (not a fork's, a pull request's or a dispatch's).
pub(super) const OFFER_IF: &str = "${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository }}";

/// The `if:` of the `deploy` job, and of `check`: that, and the offer staged a tar, and the
/// three repository variables are set (an unset one is the stop switch: the jobs are skipped,
/// which is green).
pub(super) const DEPLOY_IF: &str = "${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository && needs.offer.outputs.staged == 'true' && vars.GAMMA_HOST != '' && vars.GAMMA_USER != '' && vars.TS_TAG != '' }}";

/// The scopes each job has, all at `read`, in the order the lint sorts them.
const PERMISSIONS: [(&str, &[&str]); 3] = [
    ("offer", &["actions", "attestations", "contents"]),
    ("deploy", &["contents"]),
    ("check", &["contents"]),
];

/// What each job uploads: `(job, artifact name, path)`.
const UPLOADS: [(&str, &str, &str); 2] = [
    ("offer", "gamma-offer", "${{ runner.temp }}/offer"),
    ("deploy", "gamma-summary", "summary.json"),
];

/// The step that builds `cargo xtask`, which comes before any step that holds a credential.
const BUILD: &str = "cargo build -p xtask --locked";

/// The step that installs the toolchain.
const TOOLCHAIN: &str = "rustup toolchain install";

/// The jobs that join the tailnet, each after [`BUILD`].
const TAILNET_JOBS: [&str; 2] = ["deploy", "check"];

/// The start of the script of the one step of `check` that runs komira's text.
const CHECK_VERB: &str = "cargo xtask gamma check-build ";

/// What a script that is only the verb does not hold: another command, a redirect, a
/// substitution.
const CHAINING: [&str; 8] = ["&&", ";", "|", "`", "$(", "\n", ">", "<"];

/// The environment of that step, sorted by name: the farm's buckconfig (a secret) and the
/// targets (a repository variable). Nothing else.
const CHECK_ENV: [(&str, &str); 2] = [
    (
        "GAMMA_FARM_BUCKCONFIG",
        "${{ secrets.GAMMA_FARM_BUCKCONFIG }}",
    ),
    ("TARGETS", "${{ vars.GAMMA_CHECK_TARGETS }}"),
];

/// The inputs of the one checkout of another repository, sorted by name: komira's main, in a
/// directory of its own, with no credentials kept in it.
const KOMIRA: [(&str, &str); 4] = [
    ("path", "komira-main"),
    ("persist-credentials", "false"),
    ("ref", "main"),
    ("repository", "komira-ai/komira"),
];

/// The `concurrency` block of `deploy` and of `check`, which are in one group: a check never
/// runs during a roll, and neither job is cancelled once it has started.
const GROUP: &str = "    concurrency:\n      group: gamma\n      cancel-in-progress: false\n";

/// The flag that would make the check say nothing about the farm's cache; no spelling of it
/// may be in the workflow.
const NO_CACHE: &str = "no-remote-cache";

/// The word every way of reaching the repository's secrets has: `secrets.X`, `secrets['X']`,
/// `toJSON(secrets)` and `secrets: inherit`. Counting the bare word misses none of them.
const SECRETS: &str = "secrets";

/// The variables that name the host and the account: only the deploy step may have them.
const HOST_VARS: [&str; 2] = ["GAMMA_HOST", "GAMMA_USER"];

/// The script of the one step that may have [`HOST_VARS`]: the one that runs `gamma deploy`.
const DEPLOY_VERB: &str = "gamma deploy";

/// The problems found, one message each.
#[derive(Default)]
struct Problems(Vec<String>);

impl Problems {
    /// A problem of the whole file.
    fn add(&mut self, what: &str) {
        self.0.push(format!("line 0: {what}"));
    }

    /// A problem unless `got` is `want`. A reader's error is a problem too, so a rewritten
    /// shape cannot pass by being read as nothing.
    fn same<T: PartialEq + std::fmt::Debug>(
        &mut self,
        what: &str,
        got: Result<T, String>,
        want: T,
    ) {
        match got {
            Ok(got) if got == want => {}
            Ok(got) => self.add(&format!("{what} is {got:?}, not {want:?}")),
            Err(e) => self.add(&format!("{what}: {e}")),
        }
    }
}

pub(super) fn is_workflow_run(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("workflow_run")
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn is_tailscale(step: &Step) -> bool {
    let uses = step.uses.as_deref().unwrap_or_default();
    uses.starts_with("tailscale/github-action@")
}

/// Whether `text` names a variable of [`HOST_VARS`].
fn names_host(text: &str) -> bool {
    HOST_VARS.iter().any(|var| text.contains(*var))
}

/// The value the step passes under `with:` as `key`.
fn input<'a>(step: &'a Step, key: &str) -> Option<&'a str> {
    let found = step.with.iter().find(|(k, _)| k == key);
    found.map(|(_, value)| value.as_str())
}

fn is_check_build(step: &Step) -> bool {
    let run = step.run.as_deref().unwrap_or_default();
    run.starts_with(CHECK_VERB)
}

/// A `(name, value)` entry of a step as two strs.
fn strs(entry: &(String, String)) -> (&str, &str) {
    (entry.0.as_str(), entry.1.as_str())
}

/// The entries of a step's `with:` or `env:` as `(name, value)`, sorted by name.
fn sorted(list: &[(String, String)]) -> Vec<(&str, &str)> {
    let mut pairs: Vec<(&str, &str)> = list.iter().map(strs).collect();
    pairs.sort_unstable();
    pairs
}

/// Whether the step is the checkout of komira's main, with exactly the inputs of [`KOMIRA`].
fn is_komira_checkout(step: &Step) -> bool {
    let uses = step.uses.as_deref().unwrap_or_default();
    uses.starts_with("actions/checkout@") && sorted(&step.with) == KOMIRA
}

/// Whether a script is the `check-build` verb and nothing chained to it.
fn is_only_the_verb(run: &str) -> bool {
    let chained = CHAINING.iter().any(|c| run.contains(*c));
    run.starts_with(CHECK_VERB) && !chained
}

/// How many times the word `secrets` is in `values`.
fn secrets_in<'a>(values: impl Iterator<Item = &'a String>) -> usize {
    values.map(|v| v.matches(SECRETS).count()).sum()
}

/// How many times the word `secrets` is passed where it may be: under `with:` of the
/// Tailscale step of each of [`TAILNET_JOBS`], and in the `env:` of the `check-build` step.
fn secrets_held(text: &str) -> usize {
    let mut held = 0;
    for job in TAILNET_JOBS {
        let steps = job_steps(text, job).unwrap_or_default();
        let tailscale = steps.iter().filter(|s| is_tailscale(s));
        held += secrets_in(tailscale.flat_map(|s| s.with.iter().map(|(_, v)| v)));
    }
    let steps = job_steps(text, "check").unwrap_or_default();
    let farm = steps.iter().filter(|s| is_check_build(s));
    held + secrets_in(farm.flat_map(|s| s.env.iter().map(|(_, v)| v)))
}

/// The problems with the steps of `check` beyond those of every job: komira's text is checked
/// out once, after the build of `cargo xtask`, and a script of it runs only inside the one
/// `check-build` step, which has the two variables of [`CHECK_ENV`] and nothing else.
fn check_problems(p: &mut Problems, steps: &[Step]) {
    let build = steps.iter().position(|s| s.run.as_deref() == Some(BUILD));
    let komira = steps.iter().position(is_komira_checkout);
    let once = steps.iter().filter(|s| is_komira_checkout(s)).count() == 1;
    let after = matches!((build, komira), (Some(b), Some(k)) if b < k);
    if !(once && after) {
        p.add("`check` checks out komira once, after the xtask build");
    }
    let run_ok = |run: &str| run == BUILD || run == TOOLCHAIN || is_only_the_verb(run);
    let only = |s: &Step| s.run.as_deref().is_none_or(run_ok);
    let verbs = steps.iter().filter(|s| is_check_build(s)).count();
    if !steps.iter().all(only) || verbs != 1 {
        p.add("`check` runs more than the build and one check-build");
    }
    for step in steps.iter().filter(|s| is_check_build(s)) {
        if sorted(&step.env) != CHECK_ENV {
            p.add("the `check-build` env is not exactly the two it is given");
        }
    }
}

/// The problems with the steps of the job `job`.
fn step_problems(p: &mut Problems, job: &str, steps: &[Step]) {
    let build = steps.iter().position(|s| s.run.as_deref() == Some(BUILD));
    let joined = steps.iter().position(is_tailscale);
    let ordered = matches!((build, joined), (Some(b), Some(j)) if b < j);
    let tailnet = TAILNET_JOBS.contains(&job);
    if tailnet && !ordered {
        let why = format!("`{job}` must build cargo xtask before the Tailscale step");
        p.add(&why);
    }
    if !tailnet && joined.is_some() {
        let why = format!("`{job}` joins the tailnet: only `deploy` and `check` may");
        p.add(&why);
    }
    if job == "check" {
        check_problems(p, steps);
    }
    for (at, step) in steps.iter().enumerate() {
        let who = format!("step {} of `{job}`", at + 1);
        let uses = step.uses.as_deref().unwrap_or_default();
        let script = step.run.as_deref().unwrap_or_default();
        let picks = input(step, "ref").is_some() || input(step, "repository").is_some();
        let komira = job == "check" && is_komira_checkout(step);
        if uses.starts_with("actions/checkout@") && picks && !komira {
            p.add(&format!("{who} checks out a ref or a repository"));
        }
        if script.contains("github.event") {
            p.add(&format!("{who} puts an event field in a script"));
        }
        let secret = |value: &String| value.contains(SECRETS);
        let farm = job == "check" && is_check_build(step);
        let passed = step.with.iter().any(|(_, v)| secret(v));
        let in_env = step.env.iter().any(|(_, v)| secret(v));
        let held = (in_env && !farm) || script.contains(SECRETS);
        if held || (passed && !(tailnet && is_tailscale(step))) {
            p.add(&format!("{who} holds a credential"));
        }
        let named = |(k, v): &(String, String)| names_host(k) || names_host(v);
        let in_vars = step.env.iter().chain(&step.with).any(named);
        let deploy_step = job == "deploy" && script.contains(DEPLOY_VERB);
        if (in_vars || names_host(script)) && !deploy_step {
            p.add(&format!("{who} names the host or the user"));
        }
        if uses.starts_with("actions/upload-artifact@") {
            let pair = (input(step, "name"), input(step, "path"));
            let pinned = UPLOADS
                .iter()
                .any(|(j, name, path)| *j == job && pair == (Some(*name), Some(*path)));
            if !pinned {
                p.add(&format!("{who} uploads {pair:?}, which it may not"));
            }
            if input(step, "overwrite") != Some("true") {
                p.add(&format!("{who} uploads without `overwrite: true`"));
            }
        }
    }
}

/// The problems with a workflow that triggers on `workflow_run`: none for the real
/// `gamma.yml`, and for anything else, that only the workflow named `gamma` may. Nothing for a
/// workflow that does not trigger on it, or does not parse (the lint reports that).
pub(super) fn rules(text: &str) -> Vec<String> {
    let follows = events(text).is_ok_and(|on| on.iter().any(|e| is_workflow_run(e)));
    if !follows {
        return Vec::new();
    }
    if workflow_name(text) != Ok(Some(NAME.to_owned())) {
        return vec![format!("line 0: {NOT_GAMMA}")];
    }
    let mut p = Problems::default();
    p.same("the triggers", events(text), strings(&["workflow_run"]));
    let mut filters = workflow_run_filters(text);
    if let Ok(found) = &mut filters {
        found.sort();
    }
    let want = vec![
        ("branches".to_owned(), strings(&["main"])),
        ("types".to_owned(), strings(&["completed"])),
        ("workflows".to_owned(), strings(&[SOURCE])),
    ];
    p.same("the workflow_run filters", filters, want);
    let jobs = strings(&["offer", "deploy", "check"]);
    p.same("the jobs", job_names(text), jobs);
    for (job, want) in [
        ("offer", OFFER_IF),
        ("deploy", DEPLOY_IF),
        ("check", DEPLOY_IF),
    ] {
        let what = format!("the `if:` of `{job}`");
        p.same(&what, job_condition(text, job), Some(want.to_owned()));
    }
    let needs = [
        ("offer", strings(&[])),
        ("deploy", strings(&["offer"])),
        ("check", strings(&["offer", "deploy"])),
    ];
    for (job, want) in needs {
        let what = format!("the needs of `{job}`");
        p.same(&what, job_needs(text, job), want);
    }
    let gamma = Some("gamma".to_owned());
    let environments = [("offer", None), ("deploy", gamma.clone()), ("check", gamma)];
    for (job, want) in environments {
        let what = format!("the environment of `{job}`");
        p.same(&what, job_environment(text, job), want);
    }
    let staged = (
        "staged".to_owned(),
        "${{ steps.stage.outputs.staged }}".to_owned(),
    );
    let outputs = [
        ("offer", vec![staged]),
        ("deploy", Vec::new()),
        ("check", Vec::new()),
    ];
    for (job, want) in outputs {
        let what = format!("the outputs of `{job}`");
        p.same(&what, job_outputs(text, job), want);
    }
    for (job, scopes) in PERMISSIONS {
        let mut got = job_permission_levels(text, job);
        if let Ok(found) = &mut got {
            found.sort();
        }
        let read = |scope: &&str| ((*scope).to_owned(), "read".to_owned());
        let want: Vec<(String, String)> = scopes.iter().map(read).collect();
        p.same(&format!("the permissions of `{job}`"), got, want);
    }
    for job in ["offer", "deploy", "check"] {
        match job_steps(text, job) {
            Ok(steps) => step_problems(&mut p, job, &steps),
            Err(e) => p.add(&format!("the steps of `{job}`: {e}")),
        }
        let env = job_env(text, job).unwrap_or_default();
        if env.iter().any(|(k, v)| names_host(k) || names_host(v)) {
            p.add(&format!("the `env` of `{job}` has the host or the user"));
        }
    }
    // One deploy at a time is the deploy job's own `concurrency`, and the check's with it: a
    // workflow-level group would let a run whose deploy is skipped cancel a pending release.
    let top = workflow_has_key(text, "concurrency");
    p.same("a workflow-level `concurrency`", top, false);
    for (job, want) in [("offer", false), ("deploy", true), ("check", true)] {
        let what = format!("a `concurrency` in `{job}`");
        p.same(&what, job_has_key(text, job, "concurrency"), want);
    }
    // Both are in the group `gamma`, one pending at a time and none cancelled mid-roll.
    let queued = text.matches(GROUP).count();
    p.same("the jobs in the `gamma` group", Ok(queued), 2);
    if text.matches(SECRETS).count() != secrets_held(text) {
        let why = "`secrets` appears outside the Tailscale steps and the farm env";
        p.add(why);
    }
    if text.contains(NO_CACHE) {
        let why = format!("`{NO_CACHE}` appears: the check uses the farm's cache");
        p.add(&why);
    }
    if text.contains("id-token") {
        p.add("`id-token` appears: no job here signs or assumes a role");
    }
    p.0
}
