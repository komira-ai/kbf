//! macOS on a pull request (AGENTS.md, section 3, B9 of this repository): which jobs a pull
//! request runs on a macOS runner, and what such a job may hold. [`problems`] reads the workflow
//! files and reports each way they differ from these rules:
//!
//! - **The expected list.** Every job a pull request can run on a macOS runner is one of
//!   [`PR_MACOS_JOBS`], with its gate ([`Gate`]): macos-pr.yml's two jobs with none at all (no
//!   `if:`, so they run, and report, on every pull request), artifacts.yml's darwin build behind
//!   exactly its label condition ([`LABEL_IF`]), and the jobs of the three workflows that have
//!   their own `pull_request.paths` (none of them `*` or `**`). A macOS job (any case of
//!   `macos-`) of a workflow that runs on `pull_request` is a pull request's unless its `if:` is
//!   exactly [`NOT_ON_A_PULL_REQUEST`] (ci.yml's `task-macos`, which runs after the merge); a job
//!   of such a workflow that calls a local workflow with a macOS job is refused, as the lint cannot
//!   read the call's gate. A listed job that is gone, or not on a Mac, is stale.
//! - **No secret, no OIDC, no write.** A pull request's macOS job names no `secrets` context in
//!   any form (`secrets.X`, `secrets['X']`, `toJSON(secrets)`: the word `secrets` as an
//!   identifier, [`names_secrets`]) and has no `secrets:` key, nor does its workflow's top-level
//!   `env:`; it has no `environment`; every token scope it names is `read` or `none`, and
//!   `id-token` is `none`.
//! - **macos-pr.yml's shape.** It runs on `pull_request` and `merge_group`, each with no filter
//!   (a filtered-out run leaves its checks unreported, so a required check would wait for ever);
//!   its concurrency group is the pull request's number (a queue entry's own ref), and cancels the
//!   run in progress on a pull request only ([`GROUP`], [`CANCEL`]); its jobs are [`MACOS_JOBS`],
//!   each on `macos-15`, with no `needs`, and no job or step is `continue-on-error`.
//! - **The after-merge steps, all of them.** Every step of the after-merge Mac jobs
//!   [`AFTER_MERGE`] (beta.yml's `native-macos`, ci.yml's `task-macos`) but their cache steps
//!   is, field for field, a step of `macos-test` or `macos-device`, and the job `env:` of each
//!   is in `macos-device`'s. A step added to `native-macos` is added here too, or this fails.
//! - **The Linux check of the same crates.** Every crate `macos-test` tests whole is named in a
//!   `cargo check --target aarch64-apple-darwin --tests` step of ci.yml's `lint` job, so a
//!   macOS compile error in one is also named by a Linux job.
//!
//! What it does not check: that a test passes on a Mac (the jobs do), or that the crates
//! `macos-test` tests are all of the Mac code (a new Mac crate is added to the job by hand).

use super::workflows;

#[cfg(test)]
mod tests;

/// The directory of the workflow files.
const WORKFLOW_DIR: &str = ".github/workflows/";

/// The workflow of the pull request's macOS jobs, in [`WORKFLOW_DIR`].
pub const MACOS_PR: &str = "macos-pr.yml";

/// Its jobs, in file order: both on a Mac.
const MACOS_JOBS: [&str; 2] = ["macos-test", "macos-device"];

/// The runner of [`MACOS_JOBS`].
const RUNNER: &str = "macos-15";

/// The `if:` of a macOS job of a pull-request workflow that runs only after the merge.
pub const NOT_ON_A_PULL_REQUEST: &str = "${{ github.event_name != 'pull_request' }}";

/// The concurrency group of [`MACOS_PR`]: one run per pull request (a merge queue entry's ref).
pub const GROUP: &str = "macos-pr-${{ github.event.pull_request.number || github.ref }}";

/// The `cancel-in-progress` of [`MACOS_PR`]: a newer push to a pull request cancels its run; a
/// merge queue entry's run is never cancelled.
pub const CANCEL: &str = "${{ github.event_name == 'pull_request' }}";

/// What lets a pull request's macOS job run, and not on every pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// None: the job has no `if:`, and runs on every pull request.
    Always,
    /// The job's `if:` asks for the label `needs-artifacts`.
    Label,
    /// The workflow's `pull_request` trigger has a `paths` filter.
    TriggerPaths,
}

/// Every job a pull request can run on a macOS runner, `(workflow, job, gate)`.
pub const PR_MACOS_JOBS: [(&str, &str, Gate); 6] = [
    ("artifacts.yml", "darwin-arm64", Gate::Label),
    ("host-install-mac.yml", "real-mac", Gate::TriggerPaths),
    ("mac-provision.yml", "darwin-arm64", Gate::TriggerPaths),
    ("vmm-probe.yml", "darwin-arm64", Gate::TriggerPaths),
    (MACOS_PR, "macos-test", Gate::Always),
    (MACOS_PR, "macos-device", Gate::Always),
];

/// The Mac jobs that run after the merge, whose every step a pull request's macOS job runs.
pub const AFTER_MERGE: [(&str, &str); 2] = [("beta.yml", "native-macos"), ("ci.yml", "task-macos")];

/// The `if:` of artifacts.yml's darwin build: on a pull request, only with the label
/// `needs-artifacts` (`release_artifacts` holds the build jobs' label rule).
pub const LABEL_IF: &str = "${{ (github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, 'needs-artifacts')) && inputs.stage != 'sign' }}";

/// Whether `label` is a macOS runner, in any case.
fn is_macos(label: &str) -> bool {
    label.to_ascii_lowercase().starts_with("macos-")
}

/// Whether `s` names the `secrets` context in any form: the word `secrets`, in any case, not
/// part of a longer identifier and not a key's name (`secrets:`), so `secrets.X`, `secrets['X']`,
/// `toJSON(secrets)` and `${{ secrets }}`.
pub fn names_secrets(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    lower.match_indices("secrets").any(|(at, word)| {
        let before = lower[..at].chars().next_back();
        let after = lower[at + word.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(|c| ident(c) || c == ':')
    })
}

/// The workflow file a job's `uses:` names in this repository (`./.github/workflows/<file>`).
fn local_callee(uses: &str) -> Option<&str> {
    uses.strip_prefix("./")?.strip_prefix(WORKFLOW_DIR)
}

/// The text of the workflow `name` of `workflows`.
fn text<'a>(workflows: &'a [(String, String)], name: &str) -> Option<&'a str> {
    workflows
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, t)| t.as_str())
}

/// The jobs of a workflow text that run on a macOS runner.
fn macos_jobs(text: &str) -> Result<Vec<String>, String> {
    Ok(workflows::job_names(text)?
        .into_iter()
        .filter(|j| workflows::job_runs_on(text, j).is_ok_and(|l| is_macos(&l)))
        .collect())
}

/// Every way the workflows (`(file name in .github/workflows, text)`) differ from the rules of
/// this module, one sentence each.
pub fn problems(workflows: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    // A file a check cannot read is a problem of its own, and the other checks still run.
    let unread = [
        expected_list(workflows, &mut out),
        no_secrets(workflows, &mut out),
        shape(workflows, &mut out),
        after_merge(workflows, &mut out),
        cross_check(workflows, &mut out),
    ];
    out.extend(unread.into_iter().filter_map(Result::err));
    out
}

/// The expected list and its gates.
fn expected_list(workflows: &[(String, String)], out: &mut Vec<String>) -> Result<(), String> {
    for (name, text) in workflows {
        let on_pr = workflows::events(text)
            .map_err(|e| format!("{name}: {e}"))?
            .iter()
            .any(|e| e == "pull_request");
        if !on_pr {
            continue;
        }
        for job in workflows::job_names(text)? {
            let callee = workflows::job_uses(text, &job)?;
            let Some(callee) = callee.as_deref().and_then(local_callee) else {
                continue;
            };
            let calls_mac = workflows
                .iter()
                .find(|(n, _)| n == callee)
                .is_some_and(|(_, t)| macos_jobs(t).is_ok_and(|j| !j.is_empty()));
            if calls_mac {
                out.push(format!(
                    "{name}#{job} calls {callee}, which has a macOS job, on a pull request: this lint \
                     cannot gate a called job, so run it from {MACOS_PR}"
                ));
            }
        }
        for job in macos_jobs(text).map_err(|e| format!("{name}: {e}"))? {
            let condition =
                workflows::job_condition(text, &job).map_err(|e| format!("{name}: {e}"))?;
            if condition.as_deref() == Some(NOT_ON_A_PULL_REQUEST) {
                continue;
            }
            if !PR_MACOS_JOBS.iter().any(|(w, j, _)| w == name && *j == job) {
                out.push(format!(
                    "{name}#{job} runs on a Mac for a pull request and is not one of the pull request's \
                     macOS jobs (PR_MACOS_JOBS): run it in {MACOS_PR}, or give it \
                     `if: {NOT_ON_A_PULL_REQUEST}`"
                ));
            }
        }
    }
    for (name, job, gate) in PR_MACOS_JOBS {
        let Some(text) = text(workflows, name) else {
            out.push(format!(
                "{name}#{job} is listed as a pull request's macOS job, and there is no {name}"
            ));
            continue;
        };
        let on_mac = workflows::job_runs_on(text, job).is_ok_and(|l| is_macos(&l));
        if !on_mac {
            out.push(format!(
                "{name}#{job} is listed as a pull request's macOS job, and is no job on a Mac: drop it from the list"
            ));
            continue;
        }
        let condition = workflows::job_condition(text, job)?.unwrap_or_default();
        let gated = match gate {
            Gate::Always => condition.is_empty(),
            Gate::Label => condition == LABEL_IF,
            Gate::TriggerPaths => workflows::trigger_filters(text, "pull_request").is_ok_and(|f| {
                f.iter().any(|(k, v)| {
                    k == "paths" && !v.is_empty() && v.iter().all(|p| p != "*" && p != "**")
                })
            }),
        };
        if !gated {
            out.push(match gate {
                Gate::Always => format!(
                    "{name}#{job} has `if: {condition}`; it runs on every pull request, and a job its \
                     condition skips reports success without having run on a Mac"
                ),
                _ => format!(
                    "{name}#{job} would run on a Mac for every pull request: its gate ({gate:?}) is gone"
                ),
            });
        }
    }
    Ok(())
}

/// A pull request's macOS job holds no secret, environment, OIDC token or write scope.
fn no_secrets(workflows: &[(String, String)], out: &mut Vec<String>) -> Result<(), String> {
    for (name, job, _) in PR_MACOS_JOBS {
        let Some(text) = text(workflows, name) else {
            continue;
        };
        if workflows::job_names(text)?.iter().all(|j| j != job) {
            continue;
        }
        let at = format!("{name}#{job}");
        let strings = workflows::job_strings(text, job)?;
        if strings.iter().any(|s| names_secrets(s)) {
            out.push(format!(
                "{at} names a secret (the `secrets` context); a pull request's macOS job holds none"
            ));
        }
        if workflows::job_has_key(text, job, "secrets")? {
            out.push(format!(
                "{at} passes `secrets:`; a pull request's macOS job holds none"
            ));
        }
        if workflows::job_environment(text, job)?.is_some() {
            out.push(format!(
                "{at} runs in an environment, whose secrets it would hold"
            ));
        }
        for (scope, level) in workflows::job_permission_levels(text, job)? {
            let allowed: &[&str] = if scope == "id-token" {
                &["none"]
            } else {
                &["read", "none"]
            };
            if !allowed.contains(&level.as_str()) {
                out.push(format!(
                    "{at} sets `{scope}: {level}`; a pull request's macOS job may only read{}",
                    if scope == "id-token" {
                        ", and has no OIDC token"
                    } else {
                        ""
                    }
                ));
            }
        }
        if workflows::top_env(text)?
            .iter()
            .any(|(_, v)| names_secrets(v))
        {
            out.push(format!(
                "{name}'s top-level `env:` names a secret, which {at} would hold"
            ));
        }
    }
    Ok(())
}

/// The shape of [`MACOS_PR`].
fn shape(workflows: &[(String, String)], out: &mut Vec<String>) -> Result<(), String> {
    let Some(text) = text(workflows, MACOS_PR) else {
        out.push(format!("there is no {WORKFLOW_DIR}{MACOS_PR}"));
        return Ok(());
    };
    let mut say = |what: String| out.push(format!("{MACOS_PR}: {what}"));
    let events = workflows::events(text)?;
    if events != ["pull_request", "merge_group"] {
        say(format!(
            "runs on {events:?}; it runs on `pull_request` and `merge_group` alone"
        ));
    }
    for trigger in ["pull_request", "merge_group"] {
        if let Ok(filters) = workflows::trigger_filters(text, trigger)
            && !filters.is_empty()
        {
            say(format!(
                "filters its `{trigger}` trigger ({filters:?}): a run that never starts leaves its checks unreported"
            ));
        }
    }
    let concurrency = workflows::concurrency(text)?;
    if concurrency != Some((GROUP.to_owned(), CANCEL.to_owned())) {
        say(format!(
            "its concurrency is {concurrency:?}; it is the group `{GROUP}` with `cancel-in-progress: {CANCEL}`"
        ));
    }
    let jobs = workflows::job_names(text)?;
    if jobs != MACOS_JOBS {
        say(format!("its jobs are {jobs:?}, not {MACOS_JOBS:?}"));
        return Ok(());
    }
    for job in MACOS_JOBS {
        let strings = workflows::job_strings(text, job)?;
        if strings
            .iter()
            .any(|s| s.eq_ignore_ascii_case("continue-on-error"))
        {
            say(format!(
                "`{job}` has a `continue-on-error`, which lets it pass on a failure"
            ));
        }
        let runner = workflows::job_runs_on(text, job)?;
        if runner != RUNNER {
            say(format!("`{job}` runs on `{runner}`, not `{RUNNER}`"));
        }
        let needs = workflows::job_needs(text, job)?;
        if !needs.is_empty() {
            say(format!(
                "`{job}` needs {needs:?}: a failed or skipped need skips it, and a skipped required check passes"
            ));
        }
    }
    Ok(())
}

/// Every step of [`AFTER_MERGE`] but the cache steps is a step of [`MACOS_JOBS`].
fn after_merge(workflows: &[(String, String)], out: &mut Vec<String>) -> Result<(), String> {
    let Some(pr) = text(workflows, MACOS_PR) else {
        return Ok(());
    };
    let mut ours = Vec::new();
    for job in MACOS_JOBS {
        ours.extend(workflows::job_steps(pr, job)?);
    }
    let device_env = workflows::job_env(pr, "macos-device")?;
    for (name, job) in AFTER_MERGE {
        let Some(text) = text(workflows, name) else {
            out.push(format!(
                "there is no {name}, whose `{job}` the pull request's macOS jobs follow"
            ));
            continue;
        };
        for step in workflows::job_steps(text, job)? {
            if step
                .uses
                .as_deref()
                .is_some_and(|u| u.starts_with("actions/cache/"))
            {
                continue;
            }
            if !ours.contains(&step) {
                let what = step
                    .run
                    .as_deref()
                    .or(step.uses.as_deref())
                    .unwrap_or("(no run)");
                out.push(format!(
                    "{name}#{job} runs `{what}` and no step of {MACOS_PR}'s macOS jobs is that step"
                ));
            }
        }
        for pair in workflows::job_env(text, job)? {
            if !device_env.contains(&pair) {
                out.push(format!(
                    "{name}#{job} sets `{}: {}` and {MACOS_PR}'s `macos-device` does not",
                    pair.0, pair.1
                ));
            }
        }
    }
    Ok(())
}

/// What one cargo line selects: its packages, and whether it takes them whole (no `--lib`,
/// `--bin` or `--test`).
#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub packages: Vec<String>,
    pub whole: bool,
}

/// The selection of a `cargo test`, `cargo build`, `cargo check` or `cargo xtask sudo-test`
/// line, read up to its `--`; `None` for any other line.
pub fn selection(run: &str) -> Option<Selection> {
    let words: Vec<&str> = run.split_whitespace().collect();
    let rest = match words.as_slice() {
        ["cargo", "test" | "build" | "check", rest @ ..] => rest,
        ["cargo", "xtask", "sudo-test", rest @ ..] => rest,
        _ => return None,
    };
    let mut s = Selection {
        packages: Vec::new(),
        whole: true,
    };
    let mut words = rest.iter().take_while(|w| **w != "--");
    while let Some(w) = words.next() {
        match *w {
            "-p" | "--package" => s.packages.extend(words.next().map(|p| (*p).to_owned())),
            "--test" | "--bin" => {
                s.whole = false;
                words.next();
            }
            "--lib" => s.whole = false,
            _ => {}
        }
    }
    Some(s)
}

/// The selections of every run step of the job `job` of `text`.
fn selections(text: &str, job: &str) -> Result<Vec<Selection>, String> {
    Ok(workflows::job_steps(text, job)?
        .iter()
        .filter_map(|s| s.run.as_deref().and_then(selection))
        .collect())
}

/// Every crate `macos-test` tests whole is in a darwin `--tests` check of ci.yml's `lint` job.
fn cross_check(workflows: &[(String, String)], out: &mut Vec<String>) -> Result<(), String> {
    let (Some(pr), Some(ci)) = (text(workflows, MACOS_PR), text(workflows, "ci.yml")) else {
        return Ok(());
    };
    let checked: Vec<String> = workflows::job_steps(ci, "lint")?
        .iter()
        .filter_map(|s| s.run.as_deref())
        .filter(|r| r.contains("--target aarch64-apple-darwin") && r.contains(" --tests"))
        .filter_map(selection)
        .flat_map(|s| s.packages)
        .collect();
    for s in selections(pr, "macos-test")? {
        if !s.whole {
            continue;
        }
        for p in s.packages {
            if !checked.contains(&p) {
                out.push(format!(
                    "{MACOS_PR}#macos-test tests {p} whole, and ci.yml's `lint` job does not check its tests for \
                     aarch64-apple-darwin: add `-p {p}` there"
                ));
            }
        }
    }
    Ok(())
}

/// The repository's workflows, `(file name, text)`.
#[cfg(test)]
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
        text(&workflows, MACOS_PR).is_some() && text(&workflows, "beta.yml").is_some(),
        "the workflows were not found; the lint would check nothing: {:?}",
        workflows.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    workflows
}

#[test]
fn macos_runs_on_every_pull_request_and_holds_no_secret() {
    // Catches, on the real files: a pull request's macOS job outside the list or skippable, one
    // that holds a secret or a write scope, macos-pr.yml drifting from its shape, a step of
    // native-macos that no pull request runs, and a crate it tests that the Linux check misses.
    assert_eq!(problems(&real_files()), Vec::<String>::new());
}
