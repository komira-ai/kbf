//! `pipeline-rerun`: the one rerun of a pipeline run whose only failures are artifact jobs.
//!
//! A flaky download or upload of an artifact fails one job of the push pipeline
//! (`.github/workflows/pipeline.yml`), and its fail-fast (`pipeline-watch`) cancels the rest: an
//! hour of work lost for a flake. GitHub refuses to rerun a job while its run is in progress, and
//! the watch keeps the run open, so the rerun cannot be made from inside the run. It is made after
//! it: `.github/workflows/pipeline-rerun.yml` starts when a pipeline run completes and runs this.
//!
//! It reads the run and its jobs and acts only when all of this holds:
//! - the run is the first attempt (`run_attempt` 1), so it never loops: the completion of the
//!   rerun is attempt 2 and is ignored;
//! - the run is a push to `main` that did not succeed (`failure`, or `cancelled`, which is how a
//!   run ends when the watch cancels it for a failed job);
//! - at least one job failed or timed out, and EVERY one of them is on [`RERUNNABLE`]: the jobs
//!   that build or move artifacts, by exact name (`<caller job> / <job>`). A failed test, lint
//!   or smoke job, or any job nobody listed, means nothing is rerun and the pipeline stays failed,
//!   so a flaky test still fails loudly and gets fixed.
//!
//! Then it asks GitHub to rerun the failed jobs (`POST .../actions/runs/<id>/rerun-failed-jobs`)
//! once, and reports the jobs and the logs of their first failure in the summary of its own run
//! (`GITHUB_STEP_SUMMARY`), so flakes stay visible and countable. A rerun that GitHub refuses is an
//! error (exit 1): the pipeline is red already, and the red run of this workflow says the rerun
//! was wanted and not made. Every other outcome, nothing to do included, exits 0.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args as ClapArgs;
use serde_json::Value;

use crate::Error;

/// Jobs GitHub lists on one page, the most it allows.
const PAGE: usize = 100;

/// Flags of `pipeline-rerun`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The pipeline run that completed.
    #[arg(long)]
    pub run_id: u64,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// The jobs that build or move artifacts, by exact name: the only failed jobs a rerun is made
/// for. Every other job, tests and lints included, keeps the pipeline failed.
pub const RERUNNABLE: &[&str] = &[
    "artifacts / linux-x86_64",
    "artifacts / linux-arm64",
    "artifacts / darwin-arm64",
    "artifacts / sums-linux",
    "artifacts / sums-darwin-arm64",
    "artifacts / sums",
    "artifacts-sign / attest-linux",
    "artifacts-sign / attest-darwin-arm64",
    "artifacts-sign / verify-linux",
    "artifacts-sign / verify-darwin-arm64",
];

/// Whether the job called `name` may be rerun: only the jobs on [`RERUNNABLE`].
pub fn rerunnable(name: &str) -> bool {
    RERUNNABLE.contains(&name)
}

/// The facts of the pipeline run that decide whether to rerun.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub attempt: u64,
    pub conclusion: String,
    pub branch: String,
    pub event: String,
}

/// One job of the run as GitHub lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub name: String,
    pub status: String,
    pub conclusion: String,
    /// The page of the job and its log.
    pub url: String,
}

/// What to do with the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Rerun the failed jobs, which are these.
    Rerun(Vec<JobInfo>),
    /// Do nothing, for this reason.
    Nothing(String),
}

/// The path of the run.
pub fn run_path(repo: &str, run_id: u64) -> String {
    format!("repos/{repo}/actions/runs/{run_id}")
}

/// The path of one page of the run's jobs (the latest attempt of each).
pub fn jobs_path(repo: &str, run_id: u64, page: usize) -> String {
    format!("repos/{repo}/actions/runs/{run_id}/jobs?per_page={PAGE}&filter=latest&page={page}")
}

/// The path that reruns the failed jobs of the run.
pub fn rerun_path(repo: &str, run_id: u64) -> String {
    format!("repos/{repo}/actions/runs/{run_id}/rerun-failed-jobs")
}

/// The run as GitHub's answer states it; an error when the answer has no attempt.
pub fn run_of(answer: &Value) -> Result<RunInfo, Error> {
    let text = |key: &str| answer[key].as_str().unwrap_or_default().to_owned();
    let attempt = answer["run_attempt"]
        .as_u64()
        .ok_or_else(|| Error::Failed("the answer names no run_attempt".to_owned()))?;
    Ok(RunInfo {
        attempt,
        conclusion: text("conclusion"),
        branch: text("head_branch"),
        event: text("event"),
    })
}

/// The jobs on one page of GitHub's answer; an error when the answer has no list of jobs.
pub fn jobs_of(page: &Value) -> Result<Vec<JobInfo>, Error> {
    let jobs = page["jobs"]
        .as_array()
        .ok_or_else(|| Error::Failed("the answer lists no jobs".to_owned()))?;
    let text = |job: &Value, key: &str| job[key].as_str().unwrap_or_default().to_owned();
    Ok(jobs
        .iter()
        .map(|job| JobInfo {
            name: text(job, "name"),
            status: text(job, "status"),
            conclusion: text(job, "conclusion"),
            url: text(job, "html_url"),
        })
        .collect())
}

/// Whether to rerun the failed jobs of the run (see the module doc for every condition).
pub fn decide(run: &RunInfo, jobs: &[JobInfo]) -> Decision {
    if run.attempt != 1 {
        return Decision::Nothing(format!(
            "attempt {} is not the first: a rerun is made once",
            run.attempt
        ));
    }
    if run.branch != "main" || run.event != "push" {
        return Decision::Nothing(format!(
            "the run is a {} of {}, not a push to main",
            run.event, run.branch
        ));
    }
    if !matches!(run.conclusion.as_str(), "failure" | "cancelled") {
        return Decision::Nothing(format!("the run concluded {}", run.conclusion));
    }
    let failed: Vec<JobInfo> = jobs
        .iter()
        .filter(|j| {
            j.status == "completed" && matches!(j.conclusion.as_str(), "failure" | "timed_out")
        })
        .cloned()
        .collect();
    if failed.is_empty() {
        return Decision::Nothing("no job failed".to_owned());
    }
    let others: Vec<&str> = failed
        .iter()
        .filter(|j| !rerunnable(&j.name))
        .map(|j| j.name.as_str())
        .collect();
    if !others.is_empty() {
        return Decision::Nothing(format!(
            "not a job that builds or moves artifacts: {}; the pipeline stays failed",
            others.join(", ")
        ));
    }
    Decision::Rerun(failed)
}

/// The line for the summary of a rerun: the jobs and the logs of their first failure.
pub fn summary_line(failed: &[JobInfo]) -> String {
    let jobs: Vec<String> = failed
        .iter()
        .map(|j| format!("`{}` (first failure: {})", j.name, j.url))
        .collect();
    format!("- rerun of the failed jobs, once: {}", jobs.join(", "))
}

/// Every job of the run, page after page.
fn all_jobs(gh: &Path, repo: &str, run_id: u64) -> Result<Vec<JobInfo>, Error> {
    let mut jobs = Vec::new();
    for page in 1.. {
        let path = jobs_path(repo, run_id, page);
        let answer = crate::release::api(gh, &path)?
            .ok_or_else(|| Error::Failed(format!("gh api {path}: not found")))?;
        let these = jobs_of(&answer)?;
        let full = these.len() == PAGE;
        jobs.extend(these);
        if !full {
            break;
        }
    }
    Ok(jobs)
}

/// Asks GitHub to rerun the failed jobs of the run.
pub fn rerun_failed_jobs(gh: &Path, repo: &str, run_id: u64) -> Result<(), Error> {
    let path = rerun_path(repo, run_id);
    let out = Command::new(gh)
        .args(["api", "-X", "POST", &path])
        .output()
        .map_err(Error::spawn(gh.display().to_string()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(Error::Failed(format!("gh api {path}: {}", err.trim_end())))
}

/// Appends `line` to the summary file (on Actions, `GITHUB_STEP_SUMMARY`).
fn append_summary(file: &Path, line: &str) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .and_then(|mut f| writeln!(f, "{line}"))
        .map_err(Error::spawn(format!("append to {}", file.display())))
}

/// The `pipeline-rerun` verb: the lines to print.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let path = run_path(&args.repo, args.run_id);
    let answer = crate::release::api(&args.gh, &path)?
        .ok_or_else(|| Error::Failed(format!("gh api {path}: not found")))?;
    let info = run_of(&answer)?;
    let jobs = all_jobs(&args.gh, &args.repo, args.run_id)?;
    match decide(&info, &jobs) {
        Decision::Nothing(why) => Ok(vec![format!("nothing to rerun: {why}")]),
        Decision::Rerun(failed) => {
            rerun_failed_jobs(&args.gh, &args.repo, args.run_id)?;
            let line = summary_line(&failed);
            if let Some(file) = std::env::var_os("GITHUB_STEP_SUMMARY") {
                append_summary(Path::new(&file), &line)?;
            }
            Ok(vec![line])
        }
    }
}
