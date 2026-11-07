//! macOS on a pull request (AGENTS.md, section 3, B9 of this repository): exactly when a pull
//! request takes a macOS runner, and what such a job may hold. [`problems`] reads the workflow
//! files, the pattern file `tools/ci/macos-pr-paths.txt` and the files git lists, and reports
//! each way they differ from these rules:
//!
//! - **The expected list.** Every job a pull request can run on a macOS runner is one of
//!   [`PR_MACOS_JOBS`], behind its gate ([`Gate`]): macos-pr.yml's two jobs behind its paths
//!   check (`if:` exactly [`MACOS_IF`]), artifacts.yml's darwin build behind exactly its label
//!   condition ([`LABEL_IF`]), and the jobs of the three workflows that have their own
//!   `pull_request.paths` (none of them `*` or `**`). A macOS job (any case of `macos-`) of a
//!   workflow that runs on `pull_request` is a pull request's unless its `if:` is exactly
//!   [`NOT_ON_A_PULL_REQUEST`] (ci.yml's `task-macos`, which runs after the merge); a job of such
//!   a workflow that calls a local workflow with a macOS job is refused, as the lint cannot gate
//!   the call. A listed job that is gone, or not on a Mac, is stale.
//! - **No secret, no OIDC, no write.** A pull request's macOS job names no `secrets` context in
//!   any form (`secrets.X`, `secrets['X']`, `toJSON(secrets)`: the word `secrets` as an
//!   identifier, [`names_secrets`]) and has no `secrets:` key, nor does its workflow's top-level
//!   `env:`; it has no `environment`; every token scope it names is `read` or `none`, and
//!   `id-token` is `none`.
//! - **macos-pr.yml's shape.** It runs on `pull_request` and `merge_group`, each with no filter (a
//!   filter on a trigger would leave its `macos` check unreported, so a required check would wait
//!   for ever); its concurrency group is the pull request's number (a queue entry's own ref), and
//!   cancels the run in progress on a pull request only ([`GROUP`], [`CANCEL`]); its jobs are [`MACOS_PR_JOBS`]; `changes` runs on Linux under no condition, its
//!   last step is [`PR_PATHS`] with the id `paths`, and it outputs `macos` from that step; the
//!   two macOS jobs need `changes` alone and run under [`MACOS_IF`]; `macos` runs on Linux,
//!   needs the other three under `!cancelled()`, and has one conditional step, `run: "false"`
//!   under [`GATE_FAIL_IF`]; no job or step is `continue-on-error`.
//! - **The after-merge steps, all of them.** Every step of the after-merge Mac jobs
//!   [`AFTER_MERGE`] (beta.yml's `native-macos`, ci.yml's `task-macos`) but their cache steps
//!   is, field for field, a step of `macos-test` or `macos-device`, and the job `env:` of each
//!   is in `macos-device`'s. A step added to `native-macos` is added here too, or this fails.
//! - **The pattern file covers what the jobs test.** It parses (`xtask::pr_paths::parse`) and
//!   every pattern matches a file git lists. It matches every file of each crate a macOS job of
//!   macos-pr.yml or of [`AFTER_MERGE`] tests or builds whole (`-p <crate>` without `--lib`,
//!   `--bin` or `--test`), the files of each test target such a job names alone
//!   (`--test <name>`: `crates/<crate>/tests/<name>.rs` or `tests/<name>/`), some file of every
//!   other crate it names, every workflow with a macOS job, and [`ALWAYS`].
//! - **The Linux check of the same crates.** Every crate `macos-test` tests whole is named in a
//!   `cargo check --target aarch64-apple-darwin --tests` step of ci.yml's `lint` job, so a
//!   macOS compile error in one is red on every pull request, whether it runs a Mac or not.
//!
//! What it does not check: that the patterns name the whole Mac surface (a Mac crate that no
//! job tests yet is added to the file by hand), and that a test passes on a Mac (the jobs do).

use super::workflows;
use xtask::pr_paths::{self, Pattern};

#[cfg(test)]
mod tests;

/// The pattern file, relative to the root.
pub const PATTERNS: &str = "tools/ci/macos-pr-paths.txt";

/// The directory of the workflow files.
const WORKFLOW_DIR: &str = ".github/workflows/";

/// The workflow of the pull request's macOS jobs, in [`WORKFLOW_DIR`].
pub const MACOS_PR: &str = "macos-pr.yml";

/// Its jobs, in file order.
const MACOS_PR_JOBS: [&str; 4] = ["changes", "macos-test", "macos-device", "macos"];

/// The two macOS jobs of [`MACOS_PR`].
const MACOS_JOBS: [&str; 2] = ["macos-test", "macos-device"];

/// The `if:` of [`MACOS_JOBS`]: the paths check said the macOS surface changed.
pub const MACOS_IF: &str = "${{ needs.changes.outputs.macos == 'true' }}";

/// The `if:` of a macOS job of a pull-request workflow that runs only after the merge.
pub const NOT_ON_A_PULL_REQUEST: &str = "${{ github.event_name != 'pull_request' }}";

/// The last step of `changes`.
pub const PR_PATHS: &str = "cargo xtask pr-paths --base HEAD^1 --patterns tools/ci/macos-pr-paths.txt --key macos --output \"$GITHUB_OUTPUT\"";

/// The concurrency of [`MACOS_PR`]: one run per pull request, a newer push cancels the older.
pub const GROUP: &str = "macos-pr-${{ github.event.pull_request.number || github.ref }}";

/// The `cancel-in-progress` of [`MACOS_PR`]: a newer push to a pull request cancels its run; a
/// merge queue entry's run is never cancelled.
pub const CANCEL: &str = "${{ github.event_name == 'pull_request' }}";

/// The `if:` of the `macos` job's failing step.
pub const GATE_FAIL_IF: &str = "${{ needs.changes.result != 'success' || (needs.changes.outputs.macos == 'true' && (needs.macos-test.result != 'success' || needs.macos-device.result != 'success')) }}";

/// What lets a pull request's macOS job run, and not on every pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The job's `if:` is exactly [`MACOS_IF`].
    PathsCheck,
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
    (MACOS_PR, "macos-test", Gate::PathsCheck),
    (MACOS_PR, "macos-device", Gate::PathsCheck),
];

/// The Mac jobs that run after the merge, whose every step a pull request's macOS job runs.
pub const AFTER_MERGE: [(&str, &str); 2] = [("beta.yml", "native-macos"), ("ci.yml", "task-macos")];

/// The files the pattern file must match whatever the jobs test: the check itself.
pub const ALWAYS: [&str; 3] = [
    ".github/workflows/macos-pr.yml",
    PATTERNS,
    "crates/xtask/src/pr_paths.rs",
];

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

/// Every way the workflows (`(file name in .github/workflows, text)`), the pattern file's
/// text and the files git lists differ from the rules of this module, one sentence each.
pub fn problems(workflows: &[(String, String)], patterns: &str, listed: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    // A file a check cannot read is a problem of its own, and the other checks still run.
    let unread = [
        expected_list(workflows, &mut out),
        no_secrets(workflows, &mut out),
        shape(workflows, &mut out),
        after_merge(workflows, &mut out),
        coverage(workflows, patterns, listed, &mut out),
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
                     macOS jobs (PR_MACOS_JOBS): gate it on the paths check in {MACOS_PR}, or give it \
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
            Gate::PathsCheck => condition == MACOS_IF,
            Gate::Label => condition == LABEL_IF,
            Gate::TriggerPaths => workflows::trigger_filters(text, "pull_request").is_ok_and(|f| {
                f.iter().any(|(k, v)| {
                    k == "paths" && !v.is_empty() && v.iter().all(|p| p != "*" && p != "**")
                })
            }),
        };
        if !gated {
            out.push(format!(
                "{name}#{job} would run on a Mac for every pull request: its gate ({gate:?}) is gone"
            ));
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
                "filters its `{trigger}` trigger ({filters:?}): a run that never starts leaves the `macos` check unreported"
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
    if jobs != MACOS_PR_JOBS {
        say(format!("its jobs are {jobs:?}, not {MACOS_PR_JOBS:?}"));
        return Ok(());
    }
    for job in &jobs {
        let strings = workflows::job_strings(text, job)?;
        if strings
            .iter()
            .any(|s| s.eq_ignore_ascii_case("continue-on-error"))
        {
            say(format!(
                "`{job}` has a `continue-on-error`, which lets it pass on a failure"
            ));
        }
    }
    for job in ["changes", "macos"] {
        if is_macos(&workflows::job_runs_on(text, job)?) {
            say(format!("`{job}` runs on a Mac; it runs on Linux"));
        }
    }
    if let Some(c) = workflows::job_condition(text, "changes")? {
        say(format!("`changes` has `if: {c}`; it always runs"));
    }
    let steps = workflows::job_steps(text, "changes")?;
    let last = steps.last();
    if last.and_then(|s| s.run.as_deref()) != Some(PR_PATHS)
        || last.and_then(|s| s.id.as_deref()) != Some("paths")
        || last.is_some_and(|s| s.conditional)
    {
        say(format!(
            "the last step of `changes` is not `{PR_PATHS}` with the id `paths`"
        ));
    }
    let outputs = workflows::job_outputs(text, "changes")?;
    let wanted = vec![(
        "macos".to_owned(),
        "${{ steps.paths.outputs.macos }}".to_owned(),
    )];
    if outputs != wanted {
        say(format!("`changes` outputs {outputs:?}, not {wanted:?}"));
    }
    for job in MACOS_JOBS {
        if workflows::job_needs(text, job)? != ["changes"] {
            say(format!("`{job}` does not need `changes` alone"));
        }
        let c = workflows::job_condition(text, job)?;
        if c.as_deref() != Some(MACOS_IF) {
            say(format!("`{job}` runs under {c:?}, not `{MACOS_IF}`"));
        }
    }
    if workflows::job_needs(text, "macos")? != ["changes", "macos-test", "macos-device"] {
        say("`macos` does not need `changes`, `macos-test` and `macos-device`".to_owned());
    }
    let c = workflows::job_condition(text, "macos")?;
    if c.as_deref() != Some("${{ !cancelled() }}") {
        say(format!(
            "`macos` runs under {c:?}, not `${{{{ !cancelled() }}}}`: a skipped macOS job would skip it"
        ));
    }
    let gate: Vec<_> = workflows::job_steps(text, "macos")?
        .into_iter()
        .filter(|s| s.conditional)
        .collect();
    let fails = gate.len() == 1
        && gate[0].condition.as_deref() == Some(GATE_FAIL_IF)
        && gate[0].run.as_deref() == Some("false");
    if !fails {
        say(format!(
            "`macos` does not have exactly one conditional step, `run: \"false\"` under `{GATE_FAIL_IF}`"
        ));
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

/// What one cargo line selects: its packages, whether it takes them whole, and the test
/// targets it names.
#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub packages: Vec<String>,
    pub whole: bool,
    pub tests: Vec<String>,
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
        tests: Vec::new(),
    };
    let mut words = rest.iter().take_while(|w| **w != "--");
    while let Some(w) = words.next() {
        match *w {
            "-p" | "--package" => s.packages.extend(words.next().map(|p| (*p).to_owned())),
            "--test" => {
                s.whole = false;
                s.tests.extend(words.next().map(|t| (*t).to_owned()));
            }
            "--bin" => {
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

/// The pattern file covers what the jobs test.
fn coverage(
    workflows: &[(String, String)],
    patterns: &str,
    listed: &[String],
    out: &mut Vec<String>,
) -> Result<(), String> {
    let patterns: Vec<Pattern> = match pr_paths::parse(patterns) {
        Ok(p) => p,
        Err(problems) => {
            out.extend(problems.into_iter().map(|p| format!("{PATTERNS}: {p}")));
            return Ok(());
        }
    };
    for p in &patterns {
        if !listed.iter().any(|f| p.matches(f)) {
            out.push(format!(
                "{PATTERNS}: `{}` matches no file git lists",
                p.spelling()
            ));
        }
    }
    let covered = |path: &str| patterns.iter().any(|p| p.matches(path));
    for file in ALWAYS {
        if !covered(file) {
            out.push(format!(
                "{PATTERNS} does not match {file}, which the check itself is"
            ));
        }
    }
    let mut jobs: Vec<(&str, &str)> = AFTER_MERGE.to_vec();
    jobs.extend(MACOS_JOBS.iter().map(|j| (MACOS_PR, *j)));
    let mut whole: Vec<String> = Vec::new();
    let mut named: Vec<(String, Vec<String>)> = Vec::new();
    for (name, job) in jobs {
        let Some(text) = text(workflows, name) else {
            continue;
        };
        for s in selections(text, job)? {
            for p in s.packages {
                if s.whole {
                    whole.push(p.clone());
                }
                named.push((p, s.tests.clone()));
            }
        }
    }
    for (package, tests) in &named {
        let dir = format!("crates/{package}/");
        let files: Vec<&String> = listed.iter().filter(|f| f.starts_with(&dir)).collect();
        if files.is_empty() {
            out.push(format!(
                "a macOS job names `-p {package}`, and git lists nothing under {dir}"
            ));
            continue;
        }
        if whole.contains(package) {
            if let Some(missed) = files.iter().find(|f| !covered(f)) {
                out.push(format!(
                    "{PATTERNS} does not match {missed}, of {package}, which a macOS job tests whole: add `{dir}**`"
                ));
            }
            continue;
        }
        if !files.iter().any(|f| covered(f)) {
            out.push(format!(
                "{PATTERNS} matches no file of {package}, which a macOS job builds or tests"
            ));
        }
        for test in tests {
            let target = format!("{dir}tests/{test}");
            let of_it: Vec<&&String> = files
                .iter()
                .filter(|f| **f == &format!("{target}.rs") || f.starts_with(&format!("{target}/")))
                .collect();
            if of_it.is_empty() {
                out.push(format!(
                    "a macOS job runs `--test {test}` of {package}, and git lists no {target}.rs"
                ));
            } else if let Some(missed) = of_it.iter().find(|f| !covered(f)) {
                out.push(format!(
                    "{PATTERNS} does not match {missed}, the test target `{test}` a macOS job runs"
                ));
            }
        }
    }
    for (name, text) in workflows {
        if !macos_jobs(text)?.is_empty() && !covered(&format!("{WORKFLOW_DIR}{name}")) {
            out.push(format!(
                "{PATTERNS} does not match {WORKFLOW_DIR}{name}, which has a macOS job"
            ));
        }
    }
    Ok(())
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

/// The repository's workflows (`(file name, text)`), its pattern file and the files git lists.
#[cfg(test)]
fn real_files() -> (Vec<(String, String)>, String, Vec<String>) {
    let root = super::repo_root();
    let listed = super::listed_files(&root);
    let workflows: Vec<(String, String)> = listed
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
    let patterns = std::fs::read_to_string(root.join(PATTERNS)).expect(PATTERNS);
    (workflows, patterns, listed)
}

#[test]
fn macos_runs_on_a_pull_request_exactly_when_the_rules_say() {
    // Catches, on the real files: a pull request's macOS job outside the list or ungated, one that
    // holds a secret or a write scope, macos-pr.yml drifting from its shape, a step of native-macos
    // that no pull request runs, and a crate a macOS job tests that the paths file misses.
    let (workflows, patterns, listed) = real_files();
    assert_eq!(
        problems(&workflows, &patterns, &listed),
        Vec::<String>::new()
    );
}
