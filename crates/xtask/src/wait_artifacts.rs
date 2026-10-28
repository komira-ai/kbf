//! `wait-artifacts`: waits until the other parts of a split job have uploaded their
//! artifacts, or one of them has failed.
//!
//! The coverage run of `ci.yml` is split into parts that run at the same time. Each part
//! builds the instrumented workspace and runs its share of the tests; the last part also
//! needs the others' raw profiles, to report on all of them with the binaries it built
//! (only a machine that built them can read a profile). GitHub has no way for a job to wait
//! for its siblings, so this verb looks at the run: `--of <n>` parts in all, so every part
//! but this one: it returns when that many artifacts of the run have a name beginning
//! `--artifact-prefix` and have not expired, and fails at once when a job whose name
//! contains `--job-prefix` has failed, was cancelled or timed out (its artifact will never
//! come, and waiting for it would hold a runner until the limit). It looks every
//! `--poll-seconds`, at most `--polls` times, and fails when they are used up.
//!
//! GitHub is spoken to with `gh api` ([`crate::advisories::Process`]); the job's token needs
//! the `actions: read` permission.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::Error;
use crate::advisories::{Call, Method, Process, Tools};

/// The arguments of `wait-artifacts`.
#[derive(clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The run whose artifacts and jobs are looked at (on Actions, `GITHUB_RUN_ID`).
    #[arg(long)]
    pub run_id: u64,
    /// The artifacts to wait for are those whose name begins with this.
    #[arg(long)]
    pub artifact_prefix: String,
    /// A job whose name contains this and which did not succeed ends the wait.
    #[arg(long)]
    pub job_prefix: String,
    /// How many parts there are in all; the verb waits for every part but this one.
    #[arg(long)]
    pub of: usize,
    /// Seconds between two looks at the run.
    #[arg(long, default_value_t = 15)]
    pub poll_seconds: u64,
    /// The most looks at the run (the default is an hour at the default interval).
    #[arg(long, default_value_t = 240)]
    pub polls: u32,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// What the verb asks of GitHub, so that its decisions are tested without it.
pub trait Github {
    /// The JSON reply to a GET of `path`.
    fn get(&mut self, path: &str) -> Result<Value, Error>;
    /// Wait before the next look.
    fn pause(&mut self);
}

struct Live<'a> {
    process: Process<'a>,
    seconds: u64,
}

impl Github for Live<'_> {
    fn get(&mut self, path: &str) -> Result<Value, Error> {
        let call = Call {
            method: Method::Get,
            path: path.to_owned(),
            body: None,
        };
        self.process
            .github(&call)?
            .ok_or_else(|| Error::Failed(format!("gh api {path}: not found")))
    }

    fn pause(&mut self) {
        std::thread::sleep(Duration::from_secs(self.seconds));
    }
}

/// The elements of the array under `key` of a reply; none when it is not there.
fn items<'a>(reply: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    reply
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// The first job named for the parts that did not succeed, as `name (conclusion)`.
fn failed_job(jobs: &Value, prefix: &str) -> Option<String> {
    items(jobs, "jobs").find_map(|job| {
        let name = job.get("name")?.as_str()?;
        let conclusion = job.get("conclusion")?.as_str()?;
        (name.contains(prefix) && matches!(conclusion, "failure" | "cancelled" | "timed_out"))
            .then(|| format!("{name} ({conclusion})"))
    })
}

/// How many artifacts begin with `prefix` and have not expired.
fn artifacts(reply: &Value, prefix: &str) -> usize {
    items(reply, "artifacts")
        .filter(|a| {
            let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
            let expired = a.get("expired").and_then(Value::as_bool).unwrap_or(false);
            name.starts_with(prefix) && !expired
        })
        .count()
}

/// Waits as the module docs say; the line to print when the artifacts are there.
pub fn wait(github: &mut dyn Github, args: &Args) -> Result<String, Error> {
    let run = format!("repos/{}/actions/runs/{}", args.repo, args.run_id);
    let expected = args.of.saturating_sub(1);
    let mut found = 0;
    for look in 0..args.polls {
        if look > 0 {
            github.pause();
        }
        let jobs = github.get(&format!("{run}/jobs?per_page=100"))?;
        if let Some(job) = failed_job(&jobs, &args.job_prefix) {
            return Err(Error::Failed(format!(
                "{job} did not succeed, so its {}* artifact will not come",
                args.artifact_prefix
            )));
        }
        let reply = github.get(&format!("{run}/artifacts?per_page=100"))?;
        found = artifacts(&reply, &args.artifact_prefix);
        if found >= expected {
            return Ok(format!(
                "wait-artifacts: {found} artifact(s) named {}* are there (wanted {expected})",
                args.artifact_prefix
            ));
        }
    }
    Err(Error::Failed(format!(
        "after {} look(s) at the run only {found} of {expected} artifact(s) named {}* have come",
        args.polls, args.artifact_prefix
    )))
}

/// `wait-artifacts`: the lines to print when it finished.
pub fn run(args: &Args, root: &Path, cargo: &std::ffi::OsStr) -> Result<Vec<String>, Error> {
    let mut github = Live {
        process: Process {
            cargo,
            gh: &args.gh,
            root,
        },
        seconds: args.poll_seconds,
    };
    wait(&mut github, args).map(|line| vec![line])
}

#[cfg(test)]
mod tests;
