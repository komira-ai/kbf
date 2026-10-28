//! `pipeline-watch`: the fail-fast of the push pipeline (`.github/workflows/pipeline.yml`).
//!
//! GitHub has no "cancel the rest when one job fails" across the jobs of a workflow (only
//! `fail-fast` inside one matrix), so the pipeline runs this beside its stages: it reads the
//! run's jobs every `--interval-secs` and, when any has failed or timed out, cancels the run,
//! so the jobs that are still going stop and the lane is free for the next push. A red job
//! keeps its own conclusion; the run ends cancelled, which starts no release.
//!
//! It watches until the job named `--last-job` (the pipeline's closing job, which needs every
//! stage) has completed, whatever its conclusion, or for `--max-polls` reads. It never makes the
//! run red by itself: a job list it cannot read [`MAX_MISSES`] times in a row, and the end of
//! its polls, end the watch with a notice and exit 0, and the run goes on without fail-fast.
//! Only a cancel that GitHub refuses is an error, and then a job has failed already.
//!
//! The jobs of a called workflow are listed as `<caller job> / <job>`. Only the latest attempt
//! of each job counts (`filter=latest`), so a re-run of the failed jobs is not cancelled for
//! the failure it is re-running.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args as ClapArgs;
use serde_json::Value;

use crate::Error;

/// Jobs GitHub lists on one page, the most it allows.
const PAGE: usize = 100;

/// Reads of the job list that may fail one after the other before the watch gives up.
pub const MAX_MISSES: u32 = 10;

/// Flags of `pipeline-watch`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The run to watch and to cancel (on Actions, `GITHUB_RUN_ID`).
    #[arg(long)]
    pub run_id: u64,
    /// The job whose completion ends the watch: the one that needs every stage.
    #[arg(long)]
    pub last_job: String,
    /// Seconds between two reads of the job list.
    #[arg(long, default_value_t = 30)]
    pub interval_secs: u64,
    /// The most reads before the watch ends without having seen the end of the run.
    #[arg(long, default_value_t = 300)]
    pub max_polls: u32,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// One job of the run as GitHub lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    pub status: String,
    pub conclusion: String,
}

/// What the job list says about the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The closing job has completed: nothing is left to cancel.
    Over,
    /// These jobs failed or timed out while the run is going.
    Failed(Vec<String>),
    /// Nothing failed and the run is not over.
    Running,
}

/// The path of one page of the run's jobs (the latest attempt of each).
pub fn jobs_path(repo: &str, run_id: u64, page: usize) -> String {
    format!("repos/{repo}/actions/runs/{run_id}/jobs?per_page={PAGE}&filter=latest&page={page}")
}

/// The path that cancels the run.
pub fn cancel_path(repo: &str, run_id: u64) -> String {
    format!("repos/{repo}/actions/runs/{run_id}/cancel")
}

/// The jobs on one page of GitHub's answer; an error when the answer has no list of jobs.
pub fn jobs_of(page: &Value) -> Result<Vec<Job>, Error> {
    let jobs = page["jobs"]
        .as_array()
        .ok_or_else(|| Error::Failed("the answer lists no jobs".to_owned()))?;
    let text = |job: &Value, key: &str| job[key].as_str().unwrap_or_default().to_owned();
    Ok(jobs
        .iter()
        .map(|job| Job {
            name: text(job, "name"),
            status: text(job, "status"),
            conclusion: text(job, "conclusion"),
        })
        .collect())
}

/// What `jobs` say. The end of the run wins over a failure: a closing job that has completed
/// means every job has, and there is nothing to cancel.
pub fn verdict(jobs: &[Job], last_job: &str) -> Verdict {
    if jobs
        .iter()
        .any(|j| j.name == last_job && j.status == "completed")
    {
        return Verdict::Over;
    }
    let failed: Vec<String> = jobs
        .iter()
        .filter(|j| {
            j.status == "completed" && matches!(j.conclusion.as_str(), "failure" | "timed_out")
        })
        .map(|j| j.name.clone())
        .collect();
    if failed.is_empty() {
        Verdict::Running
    } else {
        Verdict::Failed(failed)
    }
}

/// Watches until the run is over, a job has failed (then `cancel` is called) or `max_polls`
/// reads are done. `fetch` reads every job of the run, `pause` waits between two reads. The
/// lines to print.
pub fn watch(
    last_job: &str,
    max_polls: u32,
    mut fetch: impl FnMut() -> Result<Vec<Job>, Error>,
    mut cancel: impl FnMut() -> Result<(), Error>,
    mut pause: impl FnMut(),
) -> Result<Vec<String>, Error> {
    let mut misses = 0;
    for _ in 0..max_polls {
        match fetch() {
            Ok(jobs) => {
                misses = 0;
                match verdict(&jobs, last_job) {
                    Verdict::Over => {
                        return Ok(vec![format!("{last_job} has completed: the run is over")]);
                    }
                    Verdict::Failed(names) => {
                        cancel()?;
                        return Ok(vec![format!(
                            "failed: {}; the run is cancelled, so its other jobs stop",
                            names.join(", ")
                        )]);
                    }
                    Verdict::Running => {}
                }
            }
            Err(e) => {
                misses += 1;
                if misses == MAX_MISSES {
                    return Ok(vec![format!(
                        "notice: the jobs could not be read {MAX_MISSES} times in a row ({e}); the run goes on without fail-fast"
                    )]);
                }
            }
        }
        pause();
    }
    Ok(vec![format!(
        "notice: {max_polls} reads and the run is not over; the run goes on without fail-fast"
    )])
}

/// Every job of the run, page after page.
fn all_jobs(gh: &Path, repo: &str, run_id: u64) -> Result<Vec<Job>, Error> {
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

/// Cancels the run. A run that is already over or already cancelling (HTTP 409) is not an
/// error.
pub fn cancel_run(gh: &Path, repo: &str, run_id: u64) -> Result<(), Error> {
    let path = cancel_path(repo, run_id);
    let out = Command::new(gh)
        .args(["api", "-X", "POST", &path])
        .output()
        .map_err(Error::spawn(gh.display().to_string()))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.success() || err.contains("(HTTP 409)") {
        return Ok(());
    }
    Err(Error::Failed(format!("gh api {path}: {}", err.trim_end())))
}

/// The `pipeline-watch` verb: the lines to print.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    watch(
        &args.last_job,
        args.max_polls,
        || all_jobs(&args.gh, &args.repo, args.run_id),
        || cancel_run(&args.gh, &args.repo, args.run_id),
        || std::thread::sleep(std::time::Duration::from_secs(args.interval_secs)),
    )
}
