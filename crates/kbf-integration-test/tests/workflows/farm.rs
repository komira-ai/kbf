//! The one workflow that holds `id-token: write` on a pull request: `farm.yml`, the farm lane.
//!
//! Its job `build` runs `buck2 build //:ci //:sync_check` of the Cargo workspace on the build farm,
//! which is reachable only on its Tailscale network, and joins that network with the run's OIDC
//! token (workload identity federation: no stored secret). So it holds `contents: read` and
//! `id-token: write` on a pull request too, but only under its exact `if:` ([`JOB_IF`]): a push to
//! main, or a pull request from a branch of this repository that is not Dependabot's (a fork's
//! pull request gets no token and is skipped). The lane is advisory; ci.yml stays the gate.
//!
//! The lint of a single file cannot see the file's path, so the workflow is known by its `name:`
//! ([`NAME`]); the repository test `only_farm_yml_is_named_farm` holds the name to `farm.yml`. The
//! exception is keyed to that name, the job [`JOB`] and the exact `if:` ([`Lint::farm_scope_ok`]):
//! the same `if:` in another job or another workflow is refused like any other.
//!
//! [`problems`] holds the file to its shape (the rest of the job, the pinned-tool cache and the
//! xtask build, is reviewed like any change to a workflow):
//! - `on` is exactly `pull_request` with no filter and `push` to `branches: [main]` (no merge
//!   group: the lane never runs in the merge queue), and the top-level `permissions` are `{}`;
//! - the jobs are exactly [`JOB`], on [`RUNNER`], under [`JOB_IF`], with `permissions` exactly
//!   `contents: read` and `id-token: write`;
//! - `tailscale/github-action` is used by exactly one step, pinned by a 40-digit lowercase commit,
//!   whose `with:` is exactly [`JOIN_INPUTS`] (the tailnet identity from organisation variables, and
//!   the tag `tag:ci`);
//! - a step after the join runs `cargo xtask farm-config`, and a later step runs `cargo xtask
//!   farm-build` of `--target //:ci` and `--target //:sync_check`; no step before the join runs
//!   `farm-build`;
//! - the job names no `secrets` context, key or word.

use std::collections::BTreeMap;

use super::yaml::{Node, Value};
use super::{Lint, Step, get, job_names, job_steps, job_strings, top_of, yaml};

/// The `name:` of the workflow.
pub const NAME: &str = "farm";
/// Its one job, the one that holds `id-token: write` on a pull request.
pub const JOB: &str = "build";
/// The runner of [`JOB`].
pub const RUNNER: &str = "ubuntu-24.04";
/// The exact `if:` of [`JOB`].
pub const JOB_IF: &str = "${{ github.event_name == 'push' || (github.event.pull_request.head.repo.full_name == github.repository && !startsWith(github.head_ref, 'dependabot/')) }}";
/// The action that joins the farm's Tailscale network.
pub const JOIN: &str = "tailscale/github-action";
/// The `with:` of the join step, exactly.
pub const JOIN_INPUTS: [(&str, &str); 3] = [
    ("oauth-client-id", "${{ vars.TS_CLIENT_ID }}"),
    ("audience", "${{ vars.TS_AUDIENCE }}"),
    ("tags", "tag:ci"),
];
/// The command of the step that writes the farm's buckconfig.
pub const CONFIG_COMMAND: &str = "cargo xtask farm-config";
/// The command of the step that builds on the farm.
pub const BUILD_COMMAND: &str = "cargo xtask farm-build";
/// The targets the build step names, each as `--target <target>`.
pub const TARGETS: [&str; 2] = ["//:ci", "//:sync_check"];

/// What a file whose `on` is anything else is refused with.
pub const ON_PROBLEM: &str =
    "farm.yml: on must be pull_request with no filter and push to branches [main] alone";
/// What a file with any top-level permission is refused with.
pub const TOP_PERMISSIONS_PROBLEM: &str = "farm.yml: the top-level permissions must be `{}`";
/// What a job that does not run on [`RUNNER`] is refused with.
pub const RUNNER_PROBLEM: &str = "job build: runs-on must be ubuntu-24.04";
/// What a job that holds the token under another `if:` is refused with.
pub const IF_PROBLEM: &str = "job build: `id-token: write` needs the exact `if: ${{ github.event_name == 'push' || (github.event.pull_request.head.repo.full_name == github.repository && !startsWith(github.head_ref, 'dependabot/')) }}`";
/// What a job with other permissions is refused with.
pub const PERMISSIONS_PROBLEM: &str =
    "job build: permissions must be exactly contents: read and id-token: write";
/// What a join step that is not pinned by a commit is refused with.
pub const PIN_PROBLEM: &str =
    "job build: tailscale/github-action must be pinned to a 40-digit lowercase commit SHA";
/// What a job without exactly one join step is refused with.
pub const ONE_JOIN_PROBLEM: &str = "job build: exactly one step must use tailscale/github-action";
/// What a join step with other inputs is refused with.
pub const INPUTS_PROBLEM: &str = "job build: the tailscale/github-action step's `with:` must be exactly oauth-client-id: ${{ vars.TS_CLIENT_ID }}, audience: ${{ vars.TS_AUDIENCE }}, tags: tag:ci";
/// What a job with no configuring step after the join is refused with.
pub const CONFIG_PROBLEM: &str = "job build: no step after the join runs `cargo xtask farm-config`";
/// What a job with no build step after the configuring step is refused with.
pub const BUILD_PROBLEM: &str = "job build: no step after farm-config runs `cargo xtask farm-build` of --target //:ci and --target //:sync_check";
/// What a build step before the join is refused with.
pub const EARLY_BUILD_PROBLEM: &str =
    "job build: a step runs `cargo xtask farm-build` before the join of the farm's network";
/// What a job that names a secret is refused with.
pub const SECRETS_PROBLEM: &str = "job build: names `secrets`, but the join uses the run's OIDC token and the farm's organisation variables";

/// Whether the workflow is named [`NAME`].
pub(super) fn is_named(root: &Node) -> bool {
    let Value::Map(top) = &root.value else {
        return false;
    };
    get(top, "name").and_then(Node::as_str) == Some(NAME)
}

impl Lint<'_> {
    /// Whether `id-token: write` is allowed in the job `key` with the `if:` `condition`: in the
    /// job [`JOB`] of the workflow [`NAME`] under exactly [`JOB_IF`].
    pub(super) fn farm_scope_ok(&self, key: &Node, condition: Option<&str>) -> bool {
        self.farm && key.as_str() == Some(JOB) && condition == Some(JOB_IF)
    }
}

/// Whether `branches` is the list `[main]`.
fn only_main(branches: &Node) -> bool {
    matches!(&branches.value, Value::Seq(items) if matches!(&items[..], [only] if only.as_str() == Some("main")))
}

/// Whether `on` is exactly `pull_request` with no filter and `push` with `branches: [main]`.
fn on_ok(top: &[(Node, Node)]) -> bool {
    let Some(Value::Map(on)) = get(top, "on").map(|n| &n.value) else {
        return false;
    };
    let mut keys: Vec<&str> = on.iter().filter_map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    let unfiltered = get(on, "pull_request").is_some_and(|v| {
        v.is_empty_map() || (matches!(v.value, Value::Scalar { .. }) && v.as_str().is_none())
    });
    let to_main = get(on, "push").is_some_and(|push| match &push.value {
        Value::Map(m) => matches!(&m[..], [(k, branches)] if k.as_str() == Some("branches") && only_main(branches)),
        _ => false,
    });
    keys == ["pull_request", "push"] && unfiltered && to_main
}

/// Whether `uses` is [`JOIN`] at some ref.
fn is_join(step: &Step) -> bool {
    step.uses
        .as_deref()
        .and_then(|u| u.strip_prefix(JOIN))
        .is_some_and(|rest| rest.starts_with('@'))
}

/// Whether the step's `uses` is [`JOIN`] at a 40-digit lowercase commit.
fn pinned_by_commit(step: &Step) -> bool {
    step.uses
        .as_deref()
        .and_then(|u| u.strip_prefix(JOIN))
        .and_then(|rest| rest.strip_prefix('@'))
        .is_some_and(|sha| {
            sha.len() == 40 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Whether the step's `run:` starts the command `command` as its own words.
fn runs(step: &Step, command: &str) -> bool {
    step.run.as_deref().is_some_and(|run| {
        let words: Vec<&str> = run.split_whitespace().collect();
        let wanted: Vec<&str> = command.split_whitespace().collect();
        words.starts_with(&wanted)
    })
}

/// Whether the step runs [`BUILD_COMMAND`] of every one of [`TARGETS`].
fn builds_every_target(step: &Step) -> bool {
    let words: Vec<&str> = step
        .run
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    runs(step, BUILD_COMMAND)
        && TARGETS
            .iter()
            .all(|t| words.windows(2).any(|w| w == ["--target", *t]))
}

/// What is wrong with the shape of `farm.yml`, the workflow text `text`.
pub fn problems(text: &str) -> Vec<String> {
    let root = match yaml::load(text) {
        Ok(root) => root,
        Err(e) => return vec![e],
    };
    let Ok(top) = top_of(&root) else {
        return vec![ON_PROBLEM.to_owned()];
    };
    let mut problems = Vec::new();
    if !on_ok(top) {
        problems.push(ON_PROBLEM.to_owned());
    }
    if !get(top, "permissions").is_some_and(Node::is_empty_map) {
        problems.push(TOP_PERMISSIONS_PROBLEM.to_owned());
    }
    let jobs = job_names(text).unwrap_or_default();
    if !jobs.iter().any(|j| j == JOB) {
        problems.push(format!("farm.yml: no job {JOB}"));
    }
    for other in jobs.iter().filter(|j| *j != JOB) {
        problems.push(format!("job {other} is not a job of farm.yml"));
    }
    if jobs.iter().any(|j| j == JOB) {
        problems.extend(job_problems(text));
    }
    problems
}

/// What is wrong with the job [`JOB`] of the workflow text `text`.
fn job_problems(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if !super::job_runs_on(text, JOB).is_ok_and(|r| r == RUNNER) {
        problems.push(RUNNER_PROBLEM.to_owned());
    }
    if super::job_condition(text, JOB).ok().flatten().as_deref() != Some(JOB_IF) {
        problems.push(IF_PROBLEM.to_owned());
    }
    let mut held = super::job_permission_levels(text, JOB).unwrap_or_default();
    held.sort();
    let wanted = [("contents", "read"), ("id-token", "write")];
    if !held
        .iter()
        .map(|(s, l)| (s.as_str(), l.as_str()))
        .eq(wanted)
    {
        problems.push(PERMISSIONS_PROBLEM.to_owned());
    }
    let steps = job_steps(text, JOB).unwrap_or_default();
    let joins: Vec<usize> = (0..steps.len()).filter(|&i| is_join(&steps[i])).collect();
    match joins[..] {
        [at] => {
            if !pinned_by_commit(&steps[at]) {
                problems.push(PIN_PROBLEM.to_owned());
            }
            let wanted: BTreeMap<&str, &str> = BTreeMap::from(JOIN_INPUTS);
            let held: BTreeMap<&str, &str> = steps[at]
                .with
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            if held != wanted || steps[at].with.len() != JOIN_INPUTS.len() {
                problems.push(INPUTS_PROBLEM.to_owned());
            }
        }
        _ => problems.push(ONE_JOIN_PROBLEM.to_owned()),
    }
    let join = joins.first().copied();
    let config = steps
        .iter()
        .position(|s| runs(s, CONFIG_COMMAND))
        .filter(|&c| join.is_some_and(|j| j < c));
    if config.is_none() {
        problems.push(CONFIG_PROBLEM.to_owned());
    }
    let builds = config.is_some_and(|c| steps[c + 1..].iter().any(builds_every_target));
    if !builds {
        problems.push(BUILD_PROBLEM.to_owned());
    }
    if let Some(join) = join
        && steps[..join].iter().any(|s| runs(s, BUILD_COMMAND))
    {
        problems.push(EARLY_BUILD_PROBLEM.to_owned());
    }
    if job_strings(text, JOB)
        .unwrap_or_default()
        .iter()
        .any(|s| s.contains("secrets"))
    {
        problems.push(SECRETS_PROBLEM.to_owned());
    }
    problems
}

#[cfg(test)]
#[path = "farm_tests.rs"]
mod tests;
