//! The one `workflow_run` the workflow lint allows.
//!
//! A `workflow_run` workflow starts when another workflow completes, runs the default
//! branch's copy of the file whatever ref the other run was for, and holds this repository's
//! token and its environments' credentials. So only `gamma.yml`, the workflow named
//! `gamma`, may have one, and [`rules`] holds that file to its exact shape (every message
//! begins `line 0:`: the readers it uses give no line):
//! - the trigger is `workflow_run` alone (no dispatch, schedule, pull request or push) of
//!   the workflow `release`, type `completed`, branch `main`;
//! - the jobs are `offer` and `deploy`, each with the exact `if:` of [`OFFER_IF`] and
//!   [`DEPLOY_IF`] (a successful push run to `main` of this repository; `deploy` also needs
//!   the staged offer and the three repository variables, so an unset one is a green skip);
//! - the permissions are exactly the read scopes of [`PERMISSIONS`], and no `id-token`
//!   anywhere; `offer` has no environment and `deploy` has `gamma` and needs `offer`;
//! - `secrets.` appears only in the `with:` of the Tailscale step of `deploy`, which comes
//!   after the step that builds `cargo xtask`, and no other job joins the tailnet;
//! - a checkout names no `ref` and no `repository`, no `run:` script holds an event field
//!   (it goes through `env:`), and the artifacts uploaded are exactly those of [`UPLOADS`]:
//!   the offer's tar, and the redacted summary, never the installer's own.

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

/// The `if:` of the `deploy` job: that, and the offer staged a tar, and the three repository
/// variables are set (an unset one is the stop switch: the job is skipped, which is green).
pub(super) const DEPLOY_IF: &str = "${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository && needs.offer.outputs.staged == 'true' && vars.GAMMA_HOST != '' && vars.GAMMA_USER != '' && vars.TS_TAG != '' }}";

/// The scopes each job has, all at `read`, in the order the lint sorts them.
const PERMISSIONS: [(&str, &[&str]); 2] = [
    ("offer", &["actions", "attestations", "contents"]),
    ("deploy", &["contents"]),
];

/// What each job uploads: `(job, artifact name, path)`.
const UPLOADS: [(&str, &str, &str); 2] = [
    ("offer", "gamma-offer", "${{ runner.temp }}/offer"),
    ("deploy", "gamma-summary", "summary.json"),
];

/// The step that builds `cargo xtask`, which comes before any step that holds a credential.
const BUILD: &str = "cargo build -p xtask --locked";

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

/// The value the step passes under `with:` as `key`.
fn input<'a>(step: &'a Step, key: &str) -> Option<&'a str> {
    let found = step.with.iter().find(|(k, _)| k == key);
    found.map(|(_, value)| value.as_str())
}

/// How many `secrets.` the Tailscale step of `deploy` passes under `with:`.
fn secrets_held(text: &str) -> usize {
    let steps = job_steps(text, "deploy").unwrap_or_default();
    let tailscale = steps.iter().filter(|s| is_tailscale(s));
    let values = tailscale.flat_map(|s| s.with.iter().map(|(_, v)| v));
    values.map(|v| v.matches("secrets.").count()).sum()
}

/// The problems with the steps of the job `job`.
fn step_problems(p: &mut Problems, job: &str, steps: &[Step]) {
    let build = steps.iter().position(|s| s.run.as_deref() == Some(BUILD));
    let joined = steps.iter().position(is_tailscale);
    let ordered = matches!((build, joined), (Some(b), Some(j)) if b < j);
    if job == "deploy" && !ordered {
        p.add("`deploy` must build cargo xtask before the Tailscale step");
    }
    if job != "deploy" && joined.is_some() {
        p.add(&format!("`{job}` joins the tailnet: only `deploy` may"));
    }
    for (at, step) in steps.iter().enumerate() {
        let who = format!("step {} of `{job}`", at + 1);
        let uses = step.uses.as_deref().unwrap_or_default();
        let script = step.run.as_deref().unwrap_or_default();
        let picks = input(step, "ref").is_some() || input(step, "repository").is_some();
        if uses.starts_with("actions/checkout@") && picks {
            p.add(&format!("{who} checks out a ref or a repository"));
        }
        if script.contains("github.event") {
            p.add(&format!("{who} puts an event field in a script"));
        }
        let secret = |value: &String| value.contains("secrets.");
        let passed = step.with.iter().any(|(_, v)| secret(v));
        let held = step.env.iter().any(|(_, v)| secret(v)) || script.contains("secrets.");
        if held || (passed && !(job == "deploy" && is_tailscale(step))) {
            p.add(&format!("{who} holds a credential"));
        }
        if uses.starts_with("actions/upload-artifact@") {
            let pair = (input(step, "name"), input(step, "path"));
            let pinned = UPLOADS
                .iter()
                .any(|(j, name, path)| *j == job && pair == (Some(*name), Some(*path)));
            if !pinned {
                p.add(&format!("{who} uploads {pair:?}, which it may not"));
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
    p.same("the jobs", job_names(text), strings(&["offer", "deploy"]));
    for (job, want) in [("offer", OFFER_IF), ("deploy", DEPLOY_IF)] {
        let what = format!("the `if:` of `{job}`");
        p.same(&what, job_condition(text, job), Some(want.to_owned()));
    }
    for (job, want) in [("offer", strings(&[])), ("deploy", strings(&["offer"]))] {
        let what = format!("the needs of `{job}`");
        p.same(&what, job_needs(text, job), want);
    }
    for (job, want) in [("offer", None), ("deploy", Some("gamma".to_owned()))] {
        let what = format!("the environment of `{job}`");
        p.same(&what, job_environment(text, job), want);
    }
    let staged = (
        "staged".to_owned(),
        "${{ steps.stage.outputs.staged }}".to_owned(),
    );
    for (job, want) in [("offer", vec![staged]), ("deploy", Vec::new())] {
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
    for job in ["offer", "deploy"] {
        match job_steps(text, job) {
            Ok(steps) => step_problems(&mut p, job, &steps),
            Err(e) => p.add(&format!("the steps of `{job}`: {e}")),
        }
    }
    if text.matches("secrets.").count() != secrets_held(text) {
        p.add("`secrets.` appears outside the Tailscale step's `with:`");
    }
    if text.contains("id-token") {
        p.add("`id-token` appears: no job here signs or assumes a role");
    }
    p.0
}
