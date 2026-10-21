//! Lint of the stage file, `release/stages.yml`, against the workflow files.
//!
//! The stage file lists the release stages and, one row each, the workflow jobs that
//! belong to them. [`check`] refuses:
//! - a stage file that does not parse, has an unknown key, or lists no stage or no job
//!   (so the scan cannot pass by reading nothing);
//! - a stage whose `runs` is neither `actions` nor `operator`, a repeated stage name, and
//!   an `after` naming a stage not listed before it (no cycle can be written);
//! - a job row naming a stage the file does not list;
//! - an `actions` stage with no job, a stage marked `planned` that has a job, and a job
//!   of an `operator` stage (which runs outside Actions);
//! - a row naming a job its workflow lacks, and a job of a workflow with a row that no
//!   row lists;
//! - a trigger on a workflow with a row that no stage of that workflow's jobs allows
//!   under `events`, `pull_request` included;
//! - `id-token` or `attestations` set to `write` (or `write-all`) in any job of any
//!   workflow that is not a row marked `signing: true`, and a signing row whose job
//!   holds neither scope.
//!
//! It does not check that a job runs only for its own stage's events (the triggers are
//! compared per workflow), or what a signing job does with its token; the workflow
//! lint in `mod.rs` refuses a signing scope outside its exact `if:`.

use std::collections::BTreeSet;

use super::yaml::{self, Node, Value};
use super::{events, get};

/// The stage file's path, relative to the repository root.
pub const STAGE_FILE: &str = "release/stages.yml";

/// Directory of the workflow files, relative to the repository root.
pub const WORKFLOW_DIR: &str = ".github/workflows";

const TOP_KEYS: &[&str] = &["stages", "jobs"];
const STAGE_KEYS: &[&str] = &["name", "runs", "planned", "after", "result", "events"];
const JOB_KEYS: &[&str] = &["job", "stage", "signing"];
const RUNS: &[&str] = &["actions", "operator"];
/// The token scopes only a signing job may write.
const SIGNING_SCOPES: &[&str] = &["id-token", "attestations"];

struct Stage {
    name: String,
    actions: bool,
    planned: bool,
    events: Vec<String>,
    line: usize,
}

struct Row {
    workflow: String,
    job: String,
    stage: String,
    signing: bool,
    line: usize,
}

/// Checks the stage file text against the workflow files, each `(file name inside
/// [`WORKFLOW_DIR`], text)`. One message per problem.
pub fn check(stage_text: &str, workflows: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    let (stages, rows) = parse(stage_text, &mut out);
    let known = |name: &str| stages.iter().any(|s| s.name == name);
    for row in &rows {
        if !known(&row.stage) {
            out.push(format!(
                "{STAGE_FILE}: line {}: job `{}#{}` names the unknown stage `{}`",
                row.line, row.workflow, row.job, row.stage
            ));
        }
    }
    for s in &stages {
        let jobs = rows.iter().filter(|r| r.stage == s.name).count();
        let problem = match (s.actions, s.planned, jobs) {
            (true, false, 0) => "an `actions` stage with no job",
            (true, true, 1..) => "marked `planned` but has a job; delete `planned`",
            (false, _, 1..) => "an `operator` stage runs outside Actions and has no job",
            _ => continue,
        };
        out.push(format!(
            "{STAGE_FILE}: line {}: stage `{}` is {problem}",
            s.line, s.name
        ));
    }
    let stage_workflows: BTreeSet<&str> = rows.iter().map(|r| r.workflow.as_str()).collect();
    for (file, text) in workflows {
        let at = |msg: String| format!("{WORKFLOW_DIR}/{file}: {msg}");
        let jobs = match jobs_of(text) {
            Ok(jobs) => jobs,
            Err(e) => {
                out.push(at(e));
                continue;
            }
        };
        let listed = |job: &str| rows.iter().find(|r| r.workflow == *file && r.job == job);
        if stage_workflows.contains(file.as_str()) {
            for (name, node) in &jobs {
                if listed(name).is_none() {
                    out.push(at(format!(
                        "line {}: job `{name}` is in no stage of {STAGE_FILE}",
                        node.line()
                    )));
                }
            }
            out.extend(
                trigger_problems(file, text, &stages, &rows)
                    .into_iter()
                    .map(at),
            );
        }
        for (name, node) in &jobs {
            let signing = listed(name).is_some_and(|r| r.signing);
            match (signs(node), signing) {
                (true, false) => out.push(at(format!(
                    "line {}: job `{name}` can write `id-token` or `attestations` but is not a \
                     `signing: true` row of {STAGE_FILE}",
                    node.line()
                ))),
                (false, true) => out.push(at(format!(
                    "line {}: job `{name}` is a signing row of {STAGE_FILE} but holds neither \
                     `id-token: write` nor `attestations: write`",
                    node.line()
                ))),
                _ => {}
            }
        }
    }
    for r in &rows {
        let file_jobs = workflows.iter().find(|(f, _)| *f == r.workflow);
        match file_jobs {
            None => out.push(format!(
                "{STAGE_FILE}: line {}: `{}#{}` names a workflow file that is not in {WORKFLOW_DIR}",
                r.line, r.workflow, r.job
            )),
            Some((_, text)) => {
                if jobs_of(text).is_ok_and(|jobs| jobs.iter().all(|(n, _)| *n != r.job)) {
                    out.push(format!(
                        "{STAGE_FILE}: line {}: `{}` has no job `{}`",
                        r.line, r.workflow, r.job
                    ));
                }
            }
        }
    }
    out
}

/// The workflow's triggers that no stage of its listed jobs allows.
fn trigger_problems(file: &str, text: &str, stages: &[Stage], rows: &[Row]) -> Vec<String> {
    let allowed: BTreeSet<&str> = rows
        .iter()
        .filter(|r| r.workflow == file)
        .flat_map(|r| stages.iter().filter(|s| s.name == r.stage))
        .flat_map(|s| s.events.iter().map(String::as_str))
        .collect();
    match events(text) {
        Err(e) => vec![e],
        Ok(on) => on
            .into_iter()
            .filter(|e| !allowed.contains(e.as_str()))
            .map(|e| {
                format!(
                    "the trigger `{e}` is allowed by no stage of its jobs in {STAGE_FILE} \
                     (`events:`)"
                )
            })
            .collect(),
    }
}

/// The jobs of a workflow text, `(id, job node)`, in file order.
fn jobs_of(text: &str) -> Result<Vec<(String, Node)>, String> {
    let root = yaml::load(text)?;
    let Value::Map(top) = &root.value else {
        return Err("line 0: the workflow is not a mapping".to_owned());
    };
    let Some(Value::Map(jobs)) = get(top, "jobs").map(|j| &j.value) else {
        return Err("line 0: the workflow has no `jobs` mapping".to_owned());
    };
    Ok(jobs
        .iter()
        .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), v.clone())))
        .collect())
}

/// Whether the job's own `permissions` set a signing scope (or everything) to write.
fn signs(job: &Node) -> bool {
    let Value::Map(job) = &job.value else {
        return false;
    };
    match get(job, "permissions").map(|p| &p.value) {
        Some(Value::Scalar { text, .. }) => text.eq_ignore_ascii_case("write-all"),
        Some(Value::Map(scopes)) => scopes.iter().any(|(k, v)| {
            k.as_str()
                .is_some_and(|k| SIGNING_SCOPES.iter().any(|s| s.eq_ignore_ascii_case(k)))
                && v.as_str().is_some_and(|v| v.eq_ignore_ascii_case("write"))
        }),
        _ => false,
    }
}

/// Records a problem of the stage file.
fn bad(out: &mut Vec<String>, line: usize, msg: String) {
    out.push(format!("{STAGE_FILE}: line {line}: {msg}"));
}

fn parse(text: &str, out: &mut Vec<String>) -> (Vec<Stage>, Vec<Row>) {
    let root = match yaml::load(text) {
        Ok(root) => root,
        Err(e) => {
            out.push(format!("{STAGE_FILE}: {e}"));
            return (Vec::new(), Vec::new());
        }
    };
    let Value::Map(top) = &root.value else {
        bad(out, root.line(), "the file is not a mapping".to_owned());
        return (Vec::new(), Vec::new());
    };
    unknown_keys(top, TOP_KEYS, out);
    let mut stages: Vec<Stage> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    for (key, is_stages) in [("stages", true), ("jobs", false)] {
        let items = match get(top, key).map(|n| (&n.value, n.line())) {
            Some((Value::Seq(items), _)) if !items.is_empty() => items,
            Some((_, line)) => {
                bad(out, line, format!("`{key}` is not a non-empty list"));
                continue;
            }
            None => {
                bad(out, root.line(), format!("no `{key}` list"));
                continue;
            }
        };
        for item in items {
            let Value::Map(m) = &item.value else {
                bad(out, item.line(), "an entry is not a mapping".to_owned());
                continue;
            };
            if is_stages {
                unknown_keys(m, STAGE_KEYS, out);
                let name = text_of(m, "name");
                let runs = text_of(m, "runs");
                if name.is_empty() || get(m, "result").and_then(Node::as_str).is_none() {
                    bad(
                        out,
                        item.line(),
                        "a stage needs a `name` and a `result`".to_owned(),
                    );
                }
                if !RUNS.contains(&runs.as_str()) {
                    bad(
                        out,
                        item.line(),
                        format!("`runs: {runs}` is not one of {RUNS:?}"),
                    );
                }
                if stages.iter().any(|s| s.name == name) {
                    bad(out, item.line(), format!("stage `{name}` is listed twice"));
                }
                match list_of(m, "after") {
                    None => bad(
                        out,
                        item.line(),
                        format!("stage `{name}` needs an `after:` list"),
                    ),
                    Some(after) => {
                        for a in after
                            .iter()
                            .filter(|a| !stages.iter().any(|s| s.name == **a))
                        {
                            bad(
                                out,
                                item.line(),
                                format!(
                                    "stage `{name}` comes after `{a}`, which is not listed before it"
                                ),
                            );
                        }
                    }
                }
                stages.push(Stage {
                    name,
                    actions: runs == "actions",
                    planned: flag(m, "planned", item.line(), out),
                    events: list_of(m, "events").unwrap_or_default(),
                    line: item.line(),
                });
            } else {
                unknown_keys(m, JOB_KEYS, out);
                let job = text_of(m, "job");
                let (workflow, id) = job.split_once('#').unwrap_or_default();
                if workflow.is_empty() || id.is_empty() || workflow.contains('/') {
                    bad(
                        out,
                        item.line(),
                        format!("`job: {job}` is not `<workflow file>#<job id>`"),
                    );
                }
                if rows.iter().any(|r| r.workflow == workflow && r.job == id) {
                    bad(out, item.line(), format!("job `{job}` is listed twice"));
                }
                rows.push(Row {
                    workflow: workflow.to_owned(),
                    job: id.to_owned(),
                    stage: text_of(m, "stage"),
                    signing: flag(m, "signing", item.line(), out),
                    line: item.line(),
                });
            }
        }
    }
    (stages, rows)
}

fn unknown_keys(m: &[(Node, Node)], keys: &[&str], out: &mut Vec<String>) {
    for (k, _) in m {
        if !k.as_str().is_some_and(|k| keys.contains(&k)) {
            bad(out, k.line(), "an unknown key".to_owned());
        }
    }
}

fn text_of(m: &[(Node, Node)], key: &str) -> String {
    get(m, key)
        .and_then(Node::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The strings of a list under `key`; `None` when absent or not a list of strings.
fn list_of(m: &[(Node, Node)], key: &str) -> Option<Vec<String>> {
    match &get(m, key)?.value {
        Value::Seq(items) => items
            .iter()
            .map(|n| n.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}

/// An optional `true`/`false` key; absent is false.
fn flag(m: &[(Node, Node)], key: &str, line: usize, out: &mut Vec<String>) -> bool {
    match get(m, key).map(|n| n.as_str()) {
        None => false,
        Some(Some("true")) => true,
        Some(Some("false")) => false,
        Some(_) => {
            bad(out, line, format!("`{key}` is not `true` or `false`"));
            false
        }
    }
}

#[cfg(test)]
#[path = "stages_tests.rs"]
mod tests;
