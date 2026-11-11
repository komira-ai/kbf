//! `.github/workflows/hostkeeper-desired.yml`: the run that sets the desired version of the
//! hostkeeper service `kbf-storage-and-daemon` on the hostkeeper coordinator for a published
//! release, by its targets, after each release run that published one and by hand, once
//! `cargo xtask release desired` has refused a release whose .debs the stable index does not
//! list. [`problems`] reads the file and reports each way it differs from its own header:
//!
//! - **Triggers.** Only `workflow_run` and `workflow_dispatch`; the `workflow_run` is the
//!   completion of `release` on `main` and has no other filter; the dispatch's one input is the
//!   required string `version`.
//! - **Concurrency.** The group [`GROUP`] with `cancel-in-progress: false`: a run by hand is never
//!   replaced by the completion of a release run while it waits, and no run is cancelled.
//! - **The switch.** Both jobs are `main` only and OFF unless the repository variable
//!   `KBF_SETS_DESIRED` is `true`, in their job-level `if:` ([`CHECK_IF`], [`SET_IF`]); the
//!   workflow run that started `check` succeeded, ran on `main` and came from this repository.
//! - **`check`.** Holds `contents: read` and `actions: read` and no `id-token`, needs no job,
//!   builds xtask in a step before the one that reads the token, runs [`CHECK`] as its step
//!   `desired` with `VERSION`, `RUN_ID` and `GH_TOKEN` from the input, the run and the job's
//!   token, and hands on that step's three outputs, no more.
//! - **`set-desired`.** Needs `check`, holds `contents: read` and `id-token: write`, is
//!   hostkeeper's reusable workflow ([`SET_DESIRED`]) pinned to [`CLIENT_COMMIT`] with the comment
//!   `# v`[`CLIENT_RELEASE`], and is given exactly the inputs of [`with_wanted`]: the service,
//!   what `check` handed on, the hostkeeper release the pin names, the coordinator's two
//!   repository variables, the audience, `wait: false` and the tailnet's inputs from their
//!   repository variables.
//! - **No secret.** The file names no `secrets` context outside its comments and the job that
//!   calls hostkeeper's workflow has no `secrets:` key.
//!
//! The real file must report none, and `tests` refuses a copy with one defect for each rule.

use super::workflows;

// The workflow lint's YAML reader: one reader parses every workflow file.
use super::workflows::yaml;
use yaml::{Node, Value};

#[cfg(test)]
mod tests;

/// The workflow's file, under `.github/workflows`.
pub const FILE: &str = "hostkeeper-desired.yml";

/// The `if:` of `check`: a run of `main` while the repository variable is `true`, by hand, or
/// after a run of `release.yml` that succeeded on `main` of this repository.
pub const CHECK_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.KBF_SETS_DESIRED == 'true' && (github.event_name == 'workflow_dispatch' || (github.event_name == 'workflow_run' && github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository)) }}";

/// The `if:` of `set-desired`: the gate again, and `check` chose a release.
pub const SET_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.KBF_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' }}";

/// The concurrency group: one for the runs by hand, one for the runs after a release.
pub const GROUP: &str =
    "hostkeeper-desired-${{ github.event_name == 'workflow_dispatch' && 'dispatch' || 'release' }}";

/// The command of the step `desired` of `check`.
pub const CHECK: &str = "cargo xtask release desired --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --after-run \"$RUN_ID\" --dir \"$RUNNER_TEMP/desired\" --output \"$GITHUB_OUTPUT\"";

/// The command that builds xtask before any step holds a token.
pub const BUILD: &str = "cargo build -p xtask --locked";

/// The outputs `check` hands on, each its step `desired`'s.
pub const OUTPUTS: [&str; 3] = ["set", "package-version", "targets"];

/// hostkeeper's reusable workflow that sets the desired version (the CI client).
pub const SET_DESIRED: &str = "komira-ai/hostkeeper/.github/workflows/set-desired.yml";

/// The revision of the CI client the workflow pins: the commit of hostkeeper's release
/// [`CLIENT_RELEASE`], whose hkctl makes the call. Moving it moves the `job_workflow_ref` of the
/// coordinator's allow-list entry, so it is a reviewed change here.
pub const CLIENT_COMMIT: &str = "318bcc93c9bcd009d40597c45227278742acc622";

/// The hostkeeper release of [`CLIENT_COMMIT`].
pub const CLIENT_RELEASE: &str = "0.1.242";

/// The audience of the job's OIDC token, which the coordinator's allow-list entry names.
pub const AUDIENCE: &str = "kbf-hostkeeper-desired";

/// The service the call names.
pub const SERVICE: &str = "kbf-storage-and-daemon";

/// What each input of hostkeeper's workflow must be, and no other input is given.
pub fn with_wanted() -> [(&'static str, &'static str); 14] {
    [
        ("service", SERVICE),
        ("version", "${{ needs.check.outputs.package-version }}"),
        ("targets", "${{ needs.check.outputs.targets }}"),
        ("hostkeeper_version", CLIENT_RELEASE),
        ("hostkeeper_commit", CLIENT_COMMIT),
        ("coordinator_url", "${{ vars.HOSTKEEPER_COORDINATOR_URL }}"),
        ("coordinator_ca", "${{ vars.HOSTKEEPER_COORDINATOR_CA }}"),
        ("audience", AUDIENCE),
        ("wait", "false"),
        (
            "join_tailnet",
            "${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}",
        ),
        (
            "tailnet_client_id",
            "${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID }}",
        ),
        (
            "tailnet_audience",
            "${{ vars.HOSTKEEPER_TAILNET_AUDIENCE }}",
        ),
        (
            "tailnet_coordinator",
            "${{ vars.HOSTKEEPER_TAILNET_COORDINATOR }}",
        ),
        ("tailnet_tag", "${{ vars.HOSTKEEPER_TAILNET_TAG }}"),
    ]
}

/// The value of `key` in the mapping `map`, if it is one and has the key.
fn entry<'a>(map: &'a Node, key: &str) -> Option<&'a Node> {
    match &map.value {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

/// The text at `path` (keys, one level each) under `node`.
fn at<'a>(node: &'a Node, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(node, |here, key| entry(here, key))
        .and_then(Node::as_str)
}

/// The keys of the mapping `node`, sorted; none for any other node.
fn keys(node: Option<&Node>) -> Vec<String> {
    let mut found: Vec<String> = match node.map(|n| &n.value) {
        Some(Value::Map(entries)) => entries
            .iter()
            .filter_map(|(k, _)| k.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    found.sort();
    found
}

/// A read of the workflow turned into a line of problems when it fails.
fn or_problem<T: Default>(read: Result<T, String>, out: &mut Vec<String>) -> T {
    read.unwrap_or_else(|e| {
        out.push(e);
        T::default()
    })
}

/// One line per way the triggers are not the completion of `release` on `main` and a dispatch
/// with the one input `version`.
fn trigger_problems(text: &str, doc: &Node, out: &mut Vec<String>) {
    let mut events = or_problem(workflows::events(text), out);
    events.sort();
    if events != ["workflow_dispatch", "workflow_run"] {
        out.push(format!(
            "triggers are {events:?}, not workflow_dispatch and workflow_run"
        ));
    }
    let filters = or_problem(workflows::trigger_filters(text, "workflow_run"), out);
    let wanted = [
        ("workflows", "release"),
        ("types", "completed"),
        ("branches", "main"),
    ];
    for (key, want) in wanted {
        let got = filters.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        if got != Some(&vec![want.to_owned()]) {
            out.push(format!("workflow_run.{key} is {got:?}, not [{want:?}]"));
        }
    }
    for (key, _) in &filters {
        if !wanted.iter().any(|(k, _)| k == key) {
            out.push(format!("workflow_run.{key} is a filter it does not have"));
        }
    }
    let dispatch = entry(doc, "on").and_then(|o| entry(o, "workflow_dispatch"));
    let inputs = dispatch.and_then(|w| entry(w, "inputs"));
    let listed = keys(inputs);
    if listed != ["version"] {
        out.push(format!("inputs are {listed:?}, not version alone"));
    }
    let version = inputs.and_then(|i| entry(i, "version"));
    for (key, want) in [("type", "string"), ("required", "true")] {
        let got = version.and_then(|v| at(v, &[key]));
        if got != Some(want) {
            out.push(format!("input version: {key} is {got:?}, not {want}"));
        }
    }
}

/// One line per way `check` is not the read-only check that hands on the release.
fn check_problems(text: &str, out: &mut Vec<String>) {
    let needs = or_problem(workflows::job_needs(text, "check"), out);
    if !needs.is_empty() {
        out.push(format!("check: needs {needs:?}; it needs no job"));
    }
    let mut held = or_problem(workflows::job_permission_levels(text, "check"), out);
    held.sort();
    let want: Vec<(String, String)> = [("actions", "read"), ("contents", "read")]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    if held != want {
        out.push(format!(
            "check: permissions are {held:?}, not actions: read and contents: read"
        ));
    }
    let steps = or_problem(workflows::job_steps(text, "check"), out);
    if steps.iter().any(|s| s.conditional) {
        out.push("check: a step has an `if` or `continue-on-error`".to_owned());
    }
    let desired = steps
        .iter()
        .position(|s| s.id.as_deref() == Some("desired"));
    let build = steps.iter().position(|s| s.run.as_deref() == Some(BUILD));
    if desired.and_then(|d| steps.get(d)?.run.as_deref()) != Some(CHECK) {
        out.push(format!("check: the step `desired` does not run `{CHECK}`"));
    }
    if build.is_none() || build > desired {
        out.push(format!(
            "check: `{BUILD}` is not a step before the step `desired`"
        ));
    }
    for (var, want) in [
        ("VERSION", "${{ inputs.version }}"),
        ("RUN_ID", "${{ github.event.workflow_run.id }}"),
        ("GH_TOKEN", "${{ github.token }}"),
    ] {
        let got = desired.and_then(|d| steps.get(d)?.env.iter().find(|(k, _)| k == var));
        if got.map(|(_, v)| v.as_str()) != Some(want) {
            out.push(format!(
                "check: the step `desired` does not read {var} from {want}"
            ));
        }
    }
    let outputs = or_problem(workflows::job_outputs(text, "check"), out);
    for output in OUTPUTS {
        let want = format!("${{{{ steps.desired.outputs.{output} }}}}");
        let got = outputs.iter().find(|(k, _)| k == output).map(|(_, v)| v);
        if got != Some(&want) {
            out.push(format!("check: output {output} is {got:?}, not {want}"));
        }
    }
    if outputs.len() != OUTPUTS.len() {
        out.push(format!(
            "check: it hands on {} outputs, not the {} of the step `desired`",
            outputs.len(),
            OUTPUTS.len()
        ));
    }
}

/// One line per way `set-desired` is not hostkeeper's workflow, called with the release `check`
/// handed on.
fn set_desired_problems(text: &str, doc: &Node, out: &mut Vec<String>) {
    let needs = or_problem(workflows::job_needs(text, "set-desired"), out);
    if needs != ["check"] {
        out.push(format!("set-desired: needs are {needs:?}, not [\"check\"]"));
    }
    let mut held = or_problem(workflows::job_permission_levels(text, "set-desired"), out);
    held.sort();
    let want: Vec<(String, String)> = [("contents", "read"), ("id-token", "write")]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    if held != want {
        out.push(format!(
            "set-desired: permissions are {held:?}, not contents: read and id-token: write"
        ));
    }
    let uses = or_problem(workflows::job_uses(text, "set-desired"), out);
    let want = format!("{SET_DESIRED}@{CLIENT_COMMIT}");
    if uses.as_deref() != Some(want.as_str()) {
        out.push(format!("set-desired: `uses` is not {want}"));
    }
    // The version comment of the pin, on the line of `uses:`.
    let node = entry(doc, "jobs")
        .and_then(|j| entry(j, "set-desired"))
        .and_then(|j| entry(j, "uses"));
    let release = node
        .and_then(|u| text.lines().nth(u.line() - 1))
        .and_then(|line| line.split_once(" # v"))
        .map(|(_, version)| version.trim_end());
    if release != Some(CLIENT_RELEASE) {
        out.push(format!(
            "set-desired: the pin's comment names {release:?}, not v{CLIENT_RELEASE}"
        ));
    }
    let given = or_problem(workflows::job_with(text, "set-desired"), out);
    let wanted = with_wanted();
    for (key, want) in wanted {
        let got = given
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str());
        if got != Some(want) {
            out.push(format!("set-desired: with.{key} is {got:?}, not {want:?}"));
        }
    }
    // No other input: not `host_group`, which the reusable workflow does not take, and not
    // `package_digest`, which the client refuses beside the targets.
    for (key, _) in &given {
        if !wanted.iter().any(|(k, _)| k == key) {
            out.push(format!("set-desired: with.{key} is not an input it gives"));
        }
    }
    if or_problem(workflows::job_has_key(text, "set-desired", "secrets"), out) {
        out.push("set-desired: the job has a `secrets:` key".to_owned());
    }
}

/// One line per way `text`, the file `hostkeeper-desired.yml`, is not the run its header
/// describes.
pub fn problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut out = Vec::new();
    trigger_problems(text, &doc, &mut out);
    let concurrency = or_problem(workflows::concurrency(text), &mut out);
    let wanted = Some((GROUP.to_owned(), "false".to_owned()));
    if concurrency != wanted {
        out.push(format!(
            "concurrency is {concurrency:?}, not group {GROUP} with cancel-in-progress: false"
        ));
    }
    if entry(&doc, "permissions").is_none_or(|p| !p.is_empty_map()) {
        out.push("the top level does not grant `permissions: {}`".to_owned());
    }
    let mut jobs = or_problem(workflows::job_names(text), &mut out);
    jobs.sort();
    if jobs != ["check", "set-desired"] {
        out.push(format!("jobs are {jobs:?}, not check and set-desired"));
    }
    for (name, want) in [("check", CHECK_IF), ("set-desired", SET_IF)] {
        let got = or_problem(workflows::job_condition(text, name), &mut out);
        if got.as_deref() != Some(want) {
            out.push(format!("{name}: `if` is {got:?}, not {want}"));
        }
    }
    check_problems(text, &mut out);
    set_desired_problems(text, &doc, &mut out);
    for (n, line) in text.lines().enumerate() {
        if !line.trim_start().starts_with('#') && line.contains("secrets") {
            out.push(format!("line {}: the file names a secret", n + 1));
        }
    }
    out
}

/// The text of the real file.
#[cfg(test)]
fn real_file() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows")
        .join(FILE);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
