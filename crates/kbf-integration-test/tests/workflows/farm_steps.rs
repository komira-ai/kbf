//! The steps that run buck2 against the build farm, and the time limit of every step of a
//! workflow that has one.
//!
//! A buck2 connection to the farm that dies without a word leaves buck2 waiting for an answer
//! that never comes; the step then runs to its job's timeout. komira-ai/komira's `assert_remote
//! run` (the tool of its `buck2-farm` action) keeps a watchdog on the runner's connections to the
//! farm, ends a run whose connections have heard nothing from the farm past its bound and runs it
//! again. This repository uses no action of komira's (AGENTS.md B10), so its workflows check out
//! that tool's sources at a pinned commit, build it, and hand it to the two verbs that run buck2
//! against the farm, [`FARM_VERBS`].
//!
//! [`problems`] refuses, in any workflow:
//! - a step whose `run:` starts buck2 itself (its first word, quotes taken off, is `buck2` or
//!   ends in `/buck2`): buck2 reaches the farm only through a farm verb;
//! - a farm step (one whose `run:` starts a farm verb) without `--assert-remote` [`ASSERT_REMOTE`];
//! - a checkout of [`KOMIRA`] (`actions/checkout` with `repository: komira-ai/komira`) whose
//!   `with:` is not exactly [`KOMIRA_INPUTS`] and a `ref:` that is a full 40-digit lowercase
//!   commit: the sources of `assert_remote` alone, pinned, into `buck-out/` (which git and buck2
//!   ignore), with no credentials left behind;
//! - a job with a farm step and no earlier checkout of [`KOMIRA`], or no earlier step that builds
//!   [`ASSERT_REMOTE`] (its `run:` holds `build-exe` and [`EMIT`]);
//!
//! and, in a workflow with a farm step, for every job that has steps (a job that calls a reusable
//! workflow has none of its own):
//! - a job without a whole-number `timeout-minutes`;
//! - a step without a `timeout-minutes` of its own that is a whole number from 1 and less than its
//!   job's: a step the watchdog does not watch is cut by its own bound, never by the job's.

use super::{Step, job_names, job_scalar, job_steps};

/// The verbs that run buck2 against the farm.
pub const FARM_VERBS: [&str; 2] = ["cargo xtask farm-build", "cargo xtask prove-farm"];

/// Where the workflow builds `assert_remote`, as each farm step names it.
pub const ASSERT_REMOTE: &str = "\"$RUNNER_TEMP/assert_remote\"";

/// The flag of the build step that writes `assert_remote` to [`ASSERT_REMOTE`].
pub const EMIT: &str = "-femit-bin=\"$RUNNER_TEMP/assert_remote\"";

/// The repository whose `assert_remote` the farm steps run.
pub const KOMIRA: &str = "komira-ai/komira";

/// The `with:` of a checkout of [`KOMIRA`] but its `ref:`, exactly.
pub const KOMIRA_INPUTS: [(&str, &str); 5] = [
    ("repository", KOMIRA),
    ("path", "buck-out/komira"),
    ("persist-credentials", "false"),
    ("sparse-checkout", "/tools/build/ci/assert_remote/"),
    ("sparse-checkout-cone-mode", "false"),
];

/// The words of a step's `run:`, split on whitespace.
fn words(step: &Step) -> Vec<&str> {
    step.run
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .collect()
}

/// The farm verb the step's `run:` starts, if it starts one.
pub fn farm_verb(step: &Step) -> Option<&'static str> {
    let words = words(step);
    FARM_VERBS.into_iter().find(|verb| {
        let wanted: Vec<&str> = verb.split_whitespace().collect();
        words.starts_with(&wanted)
    })
}

/// Whether the step's `run:` starts buck2 itself.
fn runs_buck2(step: &Step) -> bool {
    words(step).first().is_some_and(|first| {
        let program = first.trim_matches(|c| c == '"' || c == '\'');
        program == "buck2" || program.ends_with("/buck2")
    })
}

/// The step's `with:` value of `key`.
fn input<'a>(step: &'a Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Whether the step checks out [`KOMIRA`].
pub fn is_komira_checkout(step: &Step) -> bool {
    step.uses
        .as_deref()
        .is_some_and(|u| u.starts_with("actions/checkout@"))
        && input(step, "repository") == Some(KOMIRA)
}

/// Whether a checkout of [`KOMIRA`] has exactly [`KOMIRA_INPUTS`] and a full lowercase commit as
/// its `ref:`.
fn komira_pinned(step: &Step) -> bool {
    let pinned = input(step, "ref").is_some_and(|r| {
        r.len() == 40 && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    pinned
        && step.with.len() == KOMIRA_INPUTS.len() + 1
        && KOMIRA_INPUTS
            .iter()
            .all(|(key, want)| input(step, key) == Some(*want))
}

/// Whether the step builds [`ASSERT_REMOTE`].
fn builds_assert_remote(step: &Step) -> bool {
    let words = words(step);
    words.contains(&"build-exe") && words.contains(&EMIT)
}

/// The name of a step for a message: its `name:`, else its `uses:` or `run:`, else its place.
fn label(step: &Step, at: usize) -> String {
    step.name
        .clone()
        .or_else(|| step.uses.clone())
        .or_else(|| step.run.clone())
        .unwrap_or_else(|| format!("step {}", at + 1))
}

/// A whole number of minutes.
fn minutes(text: Option<&str>) -> Option<u64> {
    text.and_then(|t| t.parse().ok())
}

/// The jobs of `text` that have steps, each with its steps; an error when it does not parse.
fn jobs_with_steps(text: &str) -> Result<Vec<(String, Vec<Step>)>, String> {
    Ok(job_names(text)?
        .into_iter()
        .filter_map(|job| job_steps(text, &job).ok().map(|steps| (job, steps)))
        .collect())
}

/// Every farm step of the workflow text `text`, as `(job, step label)`; an error when it does not
/// parse.
pub fn farm_steps(text: &str) -> Result<Vec<(String, String)>, String> {
    Ok(jobs_with_steps(text)?
        .into_iter()
        .flat_map(|(job, steps)| {
            steps
                .iter()
                .enumerate()
                .filter(|(_, s)| farm_verb(s).is_some())
                .map(|(at, s)| (job.clone(), label(s, at)))
                .collect::<Vec<_>>()
        })
        .collect())
}

/// What is wrong with the workflow text `text` by the rules of the module, one sentence each.
pub fn problems(text: &str) -> Vec<String> {
    let jobs = match jobs_with_steps(text) {
        Ok(jobs) => jobs,
        Err(e) => return vec![e],
    };
    let mut out = Vec::new();
    for (job, steps) in &jobs {
        for (at, step) in steps.iter().enumerate() {
            let name = label(step, at);
            if is_komira_checkout(step) && !komira_pinned(step) {
                out.push(format!(
                    "{job}/{name}: the checkout of {KOMIRA} is not given exactly {KOMIRA_INPUTS:?} and a `ref:` of a full 40-digit lowercase commit"
                ));
            }
            if runs_buck2(step) {
                out.push(format!(
                    "{job}/{name}: runs buck2 itself; buck2 reaches the farm only through `cargo xtask farm-build` or `cargo xtask prove-farm`, under assert_remote's watchdog"
                ));
            }
            let Some(verb) = farm_verb(step) else {
                continue;
            };
            if !words(step)
                .windows(2)
                .any(|w| w == ["--assert-remote", ASSERT_REMOTE])
            {
                out.push(format!(
                    "{job}/{name}: `{verb}` is not given `--assert-remote {ASSERT_REMOTE}`: its buck2 would run with no watchdog on its connection to the farm"
                ));
            }
            if !steps[..at].iter().any(is_komira_checkout) {
                out.push(format!(
                    "{job}/{name}: no earlier step checks out {KOMIRA}, the pinned sources of assert_remote"
                ));
            }
            if !steps[..at].iter().any(builds_assert_remote) {
                out.push(format!(
                    "{job}/{name}: no earlier step builds assert_remote with `build-exe` and `{EMIT}`"
                ));
            }
        }
    }
    let farm = jobs
        .iter()
        .any(|(_, steps)| steps.iter().any(|s| farm_verb(s).is_some()));
    if farm {
        for (job, steps) in &jobs {
            out.extend(bounds(text, job, steps));
        }
    }
    out
}

/// The time limits of the job `job` of `text` and of each of its `steps`.
fn bounds(text: &str, job: &str, steps: &[Step]) -> Vec<String> {
    let mut out = Vec::new();
    let limit = match job_scalar(text, job, "timeout-minutes") {
        Ok(value) => minutes(value.as_deref()),
        Err(e) => return vec![e],
    };
    let Some(limit) = limit else {
        return vec![format!(
            "{job}: has no whole-number `timeout-minutes`: the workflow runs buck2 against the farm, and every job of it is bounded"
        )];
    };
    for (at, step) in steps.iter().enumerate() {
        let own = minutes(step.timeout_minutes.as_deref());
        if !own.is_some_and(|m| (1..limit).contains(&m)) {
            out.push(format!(
                "{job}/{}: its `timeout-minutes` ({}) is not a whole number from 1 under the job's {limit}: a hung step would run to the job's timeout",
                label(step, at),
                step.timeout_minutes.as_deref().unwrap_or("none")
            ));
        }
    }
    out
}

#[cfg(test)]
#[path = "farm_steps_tests.rs"]
mod tests;
