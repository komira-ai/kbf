//! What `.github/workflows/release.yml` must keep: the prod stage of docs/design/kbf-cicd.md
//! (section 8.1) as workflow rules. [`problems`] reads the workflow and reports each way
//! it differs; the real file must report none, and `tests` plants each defect on it.
//!
//! - **Main only, tag push or dry run.** The triggers are exactly a `v*` tag push (no
//!   branch filter) and `workflow_dispatch`: no `pull_request`, no schedule, no other
//!   event. `verify` runs `cargo xtask release locate` (refuses a commit not on main) and
//!   `cargo xtask release verdict` against `origin/main`, each in a step that cannot be
//!   skipped, after a checkout of the whole history so the annotated tag object and
//!   `origin/main` are there.
//! - **The verdict.** `verify` runs `release verdict` after the files are checked
//!   (`release check`), for the tag and commit it was triggered for, and passes the
//!   `set-hash` it prints out of the job; `publish` hands it to `release publish
//!   --set-hash`, so only the files the verdict names are released.
//! - **The notes.** `verify` writes the notes (`release notes --out <file>`) and uploads
//!   that file as the artifact `release-notes`, so a dry run shows them.
//! - **Who may write.** Only `publish` holds a write scope (`contents`), under the exact
//!   tag-push `if:`, so a dry run (a dispatch) publishes nothing; it also runs in the
//!   `prod` environment and needs `verify`. The kbf.dev upload (an OIDC role step) is to
//!   be added to `publish`, with its scope added to [`WRITERS`] in the same change.
//! - **The shared rules.** Actions pinned to a full commit SHA, hosted runners and an
//!   empty top-level `permissions` are the workflow lint's (`workflows`); they are
//!   repeated here so a defect planted in this file reads red from this test too.

use super::workflows;

/// The jobs and the one `if:` the publish job runs under.
const VERIFY: &str = "verify";
const PUBLISH: &str = "publish";

/// The environment whose required reviewer approves a publish.
const ENVIRONMENT: &str = "prod";

/// The events that start the workflow: a pushed tag, and a dry run by hand.
const EVENTS: &[&str] = &["push", "workflow_dispatch"];

/// The `if:` under which `publish` runs: a pushed release tag, never a dispatch.
pub const TAG_PUSH_ONLY: &str =
    "${{ github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v') }}";

/// The `(job, scope)` pairs that may be `write`. Add the kbf.dev upload's scope here.
const WRITERS: &[(&str, &str)] = &[(PUBLISH, "contents")];

/// The artifact the notes are uploaded as.
const NOTES_ARTIFACT: &str = "release-notes";

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

/// What differs between `release.yml` and the rules above: one line per difference,
/// empty when there is none. An error when the workflow cannot be read.
pub fn problems(release: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    triggers(release, &mut out)?;
    writers(release, &mut out)?;
    publish_job(release, &mut out)?;
    verify_job(release, &mut out)?;
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
        if event == "pull_request" || event.starts_with("pull_request") {
            out.push(format!(
                "the workflow runs on `{event}`: a release is cut from a tag, never a pull request"
            ));
        } else if !EVENTS.contains(&event.as_str()) {
            out.push(format!(
                "the workflow runs on `{event}`, not only {EVENTS:?}"
            ));
        }
    }
    for event in EVENTS {
        if !events.iter().any(|e| e == event) {
            out.push(format!("the workflow does not run on `{event}`"));
        }
    }
    let filters = workflows::push_filters(release)?;
    let tags_only = filters.len() == 1 && filters[0].0 == "tags" && filters[0].1 == ["v*"];
    if !tags_only {
        out.push(format!(
            "`push` is filtered by {filters:?}, not exactly `tags: ['v*']`: a branch push would cut a release"
        ));
    }
    Ok(())
}

fn writers(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    for job in [VERIFY, PUBLISH] {
        for (scope, level) in workflows::job_permission_levels(release, job)? {
            let writes = level != "read" && level != "none";
            if writes && !WRITERS.contains(&(job, scope.as_str())) {
                out.push(format!(
                    "the job `{job}` holds `{scope}: {level}`: only {WRITERS:?} may write, so a dry run could publish"
                ));
            }
        }
    }
    Ok(())
}

fn publish_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let environment = workflows::job_environment(release, PUBLISH)?;
    if environment.as_deref() != Some(ENVIRONMENT) {
        out.push(format!(
            "the job `{PUBLISH}` has environment {environment:?}, not `{ENVIRONMENT}`: no required reviewer approves the publish"
        ));
    }
    if !workflows::job_needs(release, PUBLISH)?
        .iter()
        .any(|n| n == VERIFY)
    {
        out.push(format!("the job `{PUBLISH}` does not need `{VERIFY}`"));
    }
    let condition = workflows::job_condition(release, PUBLISH)?;
    if condition.as_deref() != Some(TAG_PUSH_ONLY) {
        out.push(format!(
            "the job `{PUBLISH}` has `if: {}`, not exactly `{TAG_PUSH_ONLY}`, so a dry run may publish",
            condition.as_deref().unwrap_or("(none)")
        ));
    }
    let steps = workflows::job_steps(release, PUBLISH)?;
    match command(&steps, "cargo xtask release publish ") {
        None => out.push(format!(
            "the job `{PUBLISH}` does not run `cargo xtask release publish`"
        )),
        Some((_, step, words)) => {
            if value(&words, "--set-hash") != Some("$SET_HASH") {
                out.push("`release publish` is not given `--set-hash \"$SET_HASH\"`, so it would not refuse files the verdict does not name".to_owned());
            }
            let wanted = format!("${{{{ needs.{VERIFY}.outputs.set-hash }}}}");
            if !step
                .env
                .iter()
                .any(|(k, v)| k == "SET_HASH" && *v == wanted)
            {
                out.push(format!(
                    "the publish step does not set `SET_HASH: {wanted}`"
                ));
            }
        }
    }
    Ok(())
}

fn verify_job(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, VERIFY)?;
    let checkout = steps.iter().find(|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/checkout@"))
    });
    if !checkout.is_some_and(|s| s.with.iter().any(|(k, v)| k == "fetch-depth" && v == "0")) {
        out.push(format!("the job `{VERIFY}` does not check out with `fetch-depth: 0`, so the annotated tag object and `origin/main` are not there"));
    }
    match command(&steps, "cargo xtask release locate ") {
        Some((_, step, _)) if !step.conditional => {}
        Some(_) => out.push(format!("the `release locate` step of `{VERIFY}` is under `if:` or `continue-on-error:`")),
        None => out.push(format!("the job `{VERIFY}` does not run `cargo xtask release locate`, so a commit not on main is not refused")),
    }
    let checked = command(&steps, "cargo xtask release check ").map(|(at, ..)| at);
    match command(&steps, "cargo xtask release verdict ") {
        None => out.push(format!("the job `{VERIFY}` does not run `cargo xtask release verdict`: a tag without a gamma verdict would release")),
        Some((at, step, words)) => {
            if step.conditional {
                out.push(format!("the `release verdict` step of `{VERIFY}` is under `if:` or `continue-on-error:`"));
            }
            if checked.is_none_or(|c| at <= c) {
                out.push("`release verdict` does not come after `release check`, so it would name a set not yet verified".to_owned());
            }
            for (flag, var) in [("--tag", "$TAG"), ("--commit", "$COMMIT"), ("--output", "$GITHUB_OUTPUT")] {
                if value(&words, flag) != Some(var) {
                    out.push(format!("`release verdict` is not given `{flag} \"{var}\"`"));
                }
            }
            if value(&words, "--main-ref").is_some_and(|r| r != "origin/main") {
                out.push("`release verdict` checks against a main ref other than `origin/main`".to_owned());
            }
            let id = step.id.as_deref().unwrap_or("");
            let wanted = format!("${{{{ steps.{id}.outputs.set-hash }}}}");
            let outputs = workflows::job_outputs(release, VERIFY)?;
            if id.is_empty() || !outputs.iter().any(|(k, v)| k == "set-hash" && *v == wanted) {
                out.push(format!("the job `{VERIFY}` does not output `set-hash` from the verdict step (`{wanted}`)"));
            }
        }
    }
    match command(&steps, "cargo xtask release notes ") {
        None => out.push(format!(
            "the job `{VERIFY}` does not run `cargo xtask release notes`"
        )),
        Some((at, _, words)) => {
            let notes = value(&words, "--out").unwrap_or("");
            let uploaded = steps.iter().skip(at + 1).any(|s| {
                s.uses
                    .as_deref()
                    .is_some_and(|u| u.starts_with("actions/upload-artifact@"))
                    && s.with
                        .iter()
                        .any(|(k, v)| k == "name" && v == NOTES_ARTIFACT)
                    && s.with
                        .iter()
                        .any(|(k, v)| k == "path" && !notes.is_empty() && v == notes)
                    && !s.conditional
            });
            if !uploaded {
                out.push(format!("the notes written to `{notes}` are not uploaded, in a step after, as the artifact `{NOTES_ARTIFACT}`"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
