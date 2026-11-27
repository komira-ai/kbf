//! GitHub's merge queue for `main`: every check `main` requires reports for a queue entry, as
//! itself, and a queue run is never cancelled by another and holds no secret. [`problems`] reads
//! the workflow files and reports each way they differ from these rules:
//!
//! - **Every required check reports in the queue.** Each of [`REQUIRED`] is a job of its
//!   workflow, which runs on `merge_group` with no filter (a run that never starts leaves the
//!   check unreported, and the entry waits until the queue's timeout drops it). The job reports
//!   as its own name (no `name:` that renames it, no `strategy:` that names a check per matrix
//!   entry), has no `needs` and an `if:` that is true for a merge queue run, or none: a skipped
//!   job's check passes, so a required check that a failed need or its condition skips would
//!   let an entry merge untested.
//! - **No queue run cancels another.** In every workflow that runs on `merge_group`, the
//!   concurrency group differs between two queue entries (GitHub keeps one pending run per
//!   group and cancels the pending one a newer run replaces), and `cancel-in-progress` is false
//!   for a queue run. A job of its own `concurrency:` is refused: the lint reads the workflow's.
//! - **A queue run holds no secret.** A merge queue run runs the code of pull requests with
//!   the repository's secrets in reach (it is not a fork's pull request run), so no job of a
//!   workflow that runs on `merge_group`, nor its top-level `env:`, names the `secrets`
//!   context, and no job calls another workflow (whose jobs this lint does not read).
//!
//! The expressions are evaluated by [`expr`] for a modelled queue run; one it cannot evaluate
//! is reported as such, never passed. That nothing writes, signs or releases from a queue run
//! is the workflow lint's (`workflows`: a write scope only under an `if:` that holds on a push
//! to `main` alone); the pipeline, the release and the suites have no `merge_group` trigger
//! (`pipeline_workflow`, `release_workflow`, `suites_workflow`).
//!
//! What it does not check: the ruleset itself (it lives on GitHub, not in a file), so a check
//! added to the ruleset is added to [`REQUIRED`] by hand.

use super::macos_pr::names_secrets;
use super::workflows;

pub mod expr;
#[cfg(test)]
mod tests;

use expr::Run;

/// The directory of the workflow files.
const WORKFLOW_DIR: &str = ".github/workflows/";

/// The checks of `main`, `(workflow, job)`, the job's name being the check's: `lint` and
/// `test`, which the ruleset requires, and macos-pr.yml's two jobs, which AGENTS.md's B9 names
/// required checks.
pub const REQUIRED: [(&str, &str); 4] = [
    ("ci.yml", "lint"),
    ("ci.yml", "test"),
    ("macos-pr.yml", "macos-test"),
    ("macos-pr.yml", "macos-device"),
];

/// The trigger of a merge queue run.
const MERGE_GROUP: &str = "merge_group";

/// Two runs of two queue entries, which must not share a concurrency group.
const ENTRIES: [Run<'static>; 2] = [
    Run {
        git_ref: "refs/heads/gh-readonly-queue/main/pr-1-aaaa",
        run_id: "1001",
        sha: "1111111111111111111111111111111111111111",
    },
    Run {
        git_ref: "refs/heads/gh-readonly-queue/main/pr-2-bbbb",
        run_id: "1002",
        sha: "2222222222222222222222222222222222222222",
    },
];

/// The text of the workflow `name` of `workflows`.
fn text<'a>(workflows: &'a [(String, String)], name: &str) -> Option<&'a str> {
    workflows
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, t)| t.as_str())
}

/// Whether the workflow text runs on `merge_group`.
fn on_merge_group(text: &str) -> Result<bool, String> {
    Ok(workflows::events(text)?.iter().any(|e| e == MERGE_GROUP))
}

/// Every way `workflows` (`(file name, text)` of each file in [`WORKFLOW_DIR`]) differs from
/// the rules of this module, one line each; empty when it holds them all.
pub fn problems(workflows: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (name, job) in REQUIRED {
        if let Err(e) = required(workflows, name, job, &mut out) {
            out.push(format!("{name}: {e}"));
        }
    }
    for (name, text) in workflows {
        if let Err(e) = queue_run(name, text, &mut out) {
            out.push(format!("{name}: {e}"));
        }
    }
    out
}

/// The required check `job` of the workflow `name`.
fn required(
    workflows: &[(String, String)],
    name: &str,
    job: &str,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let Some(text) = text(workflows, name) else {
        out.push(format!(
            "there is no {WORKFLOW_DIR}{name}, whose `{job}` main requires"
        ));
        return Ok(());
    };
    let at = format!("{name}#{job}");
    if !on_merge_group(text)? {
        out.push(format!(
            "{name} does not run on `merge_group`, so the required check `{job}` never reports \
             for a merge queue entry, which waits until the queue drops it"
        ));
    } else if let Ok(filters) = workflows::trigger_filters(text, MERGE_GROUP)
        && !filters.is_empty()
    {
        out.push(format!(
            "{name} filters its `merge_group` trigger ({filters:?}): a run that never starts \
             leaves the required check `{job}` unreported"
        ));
    }
    if !workflows::job_names(text)?.iter().any(|j| j == job) {
        out.push(format!("{name} has no job `{job}`, which main requires"));
        return Ok(());
    }
    if let Some(shown) = workflows::job_name(text, job)?
        && shown != job
    {
        out.push(format!(
            "{at} reports as `{shown}`, not as the required check `{job}`"
        ));
    }
    if workflows::job_has_key(text, job, "strategy")? {
        out.push(format!(
            "{at} has a `strategy:`: a matrix names a check per entry, and none is `{job}`"
        ));
    }
    let needs = workflows::job_needs(text, job)?;
    if !needs.is_empty() {
        out.push(format!(
            "{at} needs {needs:?}: a failed or skipped need skips it, and a skipped required \
             check passes"
        ));
    }
    if let Some(cond) = workflows::job_condition(text, job)? {
        match expr::condition(&cond, &ENTRIES[0]) {
            Ok(true) => {}
            Ok(false) => out.push(format!(
                "{at} is skipped in the merge queue (`if: {cond}`), and a skipped required \
                 check passes"
            )),
            Err(e) => out.push(format!(
                "{at}: its `if: {cond}` cannot be evaluated for a merge queue run: {e}"
            )),
        }
    }
    Ok(())
}

/// The concurrency, secrets and calls of the workflow `name`, when it runs on `merge_group`.
fn queue_run(name: &str, text: &str, out: &mut Vec<String>) -> Result<(), String> {
    if !on_merge_group(text)? {
        return Ok(());
    }
    if let Some((group, cancel)) = workflows::concurrency(text)? {
        let groups: Result<Vec<String>, String> = ENTRIES
            .iter()
            .map(|run| expr::template(&group, run))
            .collect();
        match groups {
            Ok(g) if g[0] == g[1] => out.push(format!(
                "{name}: every merge queue run shares the concurrency group `{}`: GitHub keeps \
                 one pending run per group and cancels the one a newer run replaces",
                g[0]
            )),
            Ok(_) => {}
            Err(e) => out.push(format!(
                "{name}: its concurrency group `{group}` cannot be evaluated for a merge queue \
                 run: {e}"
            )),
        }
        match expr::flag(&cancel, &ENTRIES[0]) {
            Ok(false) => {}
            Ok(true) => out.push(format!(
                "{name}: `cancel-in-progress: {cancel}` cancels a merge queue run in progress"
            )),
            Err(e) => out.push(format!(
                "{name}: its `cancel-in-progress: {cancel}` cannot be evaluated for a merge \
                 queue run: {e}"
            )),
        }
    }
    if workflows::top_env(text)?
        .iter()
        .any(|(_, v)| names_secrets(v))
    {
        out.push(format!(
            "{name}'s top-level `env:` names a secret, which a merge queue run would hold"
        ));
    }
    for job in workflows::job_names(text)? {
        let at = format!("{name}#{job}");
        if workflows::job_has_key(text, &job, "concurrency")? {
            out.push(format!(
                "{at} has a `concurrency:` of its own, which this lint does not read for a \
                 merge queue run"
            ));
        }
        if let Some(callee) = workflows::job_uses(text, &job)? {
            out.push(format!(
                "{at} calls `{callee}`, whose jobs this lint does not read for a merge queue run"
            ));
        }
        if workflows::job_strings(text, &job)?
            .iter()
            .any(|s| names_secrets(s))
        {
            out.push(format!(
                "{at} names a secret, which a merge queue run would hold"
            ));
        }
    }
    Ok(())
}

/// The workflow files of this repository, by file name.
fn real_files() -> Vec<(String, String)> {
    let root = super::repo_root();
    let workflows: Vec<(String, String)> = super::listed_files(&root)
        .iter()
        .filter(|f| super::is_workflow(f))
        .filter_map(|f| {
            let text = std::fs::read_to_string(root.join(f)).ok()?;
            Some((f.strip_prefix(WORKFLOW_DIR)?.to_owned(), text))
        })
        .collect();
    assert!(
        REQUIRED
            .iter()
            .all(|(name, _)| text(&workflows, name).is_some()),
        "the workflows were not found; the lint would check nothing: {:?}",
        workflows.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    workflows
}

#[test]
fn every_required_check_reports_in_the_merge_queue_and_no_queue_run_is_cancelled() {
    // Catches, on the real files: a required check's workflow without `merge_group`, the check
    // skipped or renamed in the queue, and a queue run cancelled by a newer one or holding a
    // secret.
    assert_eq!(problems(&real_files()), Vec::<String>::new());
}
