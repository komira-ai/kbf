//! `wait-artifacts`: waits until the artifacts of the sibling jobs of a run have been
//! uploaded, or one of those jobs has failed.
//!
//! The coverage run of `ci.yml` is split into shards that run at the same time, each
//! building one core's instrumented tests and running its share of them. The `test` job
//! builds the same binaries itself (only a machine that has the instrumented binaries can
//! read a raw profile) and needs every shard's profiles before it reports. GitHub has no way
//! for a job to wait for jobs it does not depend on, so this verb looks at the run: it
//! returns when every artifact named by an `--artifact` is there and has not expired, and it
//! fails at once when a job whose name contains `--job-prefix` has failed, was cancelled or
//! timed out (its artifact will never come, and waiting for it would hold a runner until the
//! limit), or when every such job has finished and a named artifact is still missing. It
//! looks every `--poll-seconds`, at most `--polls` times, and fails when they are used up. A look
//! in which GitHub answers an error is tried again, and the wait fails with that error only when
//! [`FAILED_LOOKS`] looks in a row have failed: one bad reply is not the end of an hour's wait.
//!
//! GitHub is spoken to with `gh api` ([`crate::advisories::Process`]); the job's token needs
//! the `actions: read` permission.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::Error;
use crate::advisories::{Call, Method, Process, Tools};

/// How many looks in a row may fail (GitHub answers an error) before the wait fails with the last error.
pub const FAILED_LOOKS: u32 = 4;

/// The arguments of `wait-artifacts`.
#[derive(clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The run whose artifacts and jobs are looked at (on Actions, `GITHUB_RUN_ID`).
    #[arg(long)]
    pub run_id: u64,
    /// The exact name of an artifact to wait for; once for each.
    #[arg(long = "artifact", required = true)]
    pub artifacts: Vec<String>,
    /// A job whose name contains this and which did not succeed ends the wait.
    #[arg(long)]
    pub job_prefix: String,
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

/// The jobs whose name contains `prefix`, as `(name, conclusion)`; the conclusion is `None`
/// while the job has not finished.
fn shards<'a>(jobs: &'a Value, prefix: &str) -> Vec<(&'a str, Option<&'a str>)> {
    items(jobs, "jobs")
        .filter_map(|job| {
            let name = job.get("name")?.as_str()?;
            name.contains(prefix)
                .then(|| (name, job.get("conclusion").and_then(Value::as_str)))
        })
        .collect()
}

/// The first of the jobs that did not succeed, as `name (conclusion)`.
fn failed(shards: &[(&str, Option<&str>)]) -> Option<String> {
    shards.iter().find_map(|(name, conclusion)| {
        let conclusion = (*conclusion)?;
        matches!(conclusion, "failure" | "cancelled" | "timed_out")
            .then(|| format!("{name} ({conclusion})"))
    })
}

/// The wanted names that no live (not expired) artifact of the reply has.
fn missing(reply: &Value, wanted: &[String]) -> Vec<String> {
    let live: Vec<&str> = items(reply, "artifacts")
        .filter(|a| !a.get("expired").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|a| a.get("name")?.as_str())
        .collect();
    wanted
        .iter()
        .filter(|w| !live.contains(&w.as_str()))
        .cloned()
        .collect()
}

/// A reply, or `None` for a call that failed in a look that may be tried again: the error when
/// that is the `FAILED_LOOKS`th failed look in a row.
fn tolerate(reply: Result<Value, Error>, failed_looks: &mut u32) -> Result<Option<Value>, Error> {
    match reply {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            *failed_looks += 1;
            if *failed_looks >= FAILED_LOOKS {
                Err(e)
            } else {
                Ok(None)
            }
        }
    }
}

/// Waits as the module docs say; the line to print when the artifacts are there.
pub fn wait(github: &mut dyn Github, args: &Args) -> Result<String, Error> {
    let run = format!("repos/{}/actions/runs/{}", args.repo, args.run_id);
    let mut absent = args.artifacts.clone();
    let mut failed_looks = 0;
    for look in 0..args.polls {
        if look > 0 {
            github.pause();
        }
        let Some(jobs) = tolerate(
            github.get(&format!("{run}/jobs?per_page=100")),
            &mut failed_looks,
        )?
        else {
            continue;
        };
        let shards = shards(&jobs, &args.job_prefix);
        if let Some(job) = failed(&shards) {
            return Err(Error::Failed(format!(
                "{job} did not succeed, so its artifact will not come"
            )));
        }
        let Some(reply) = tolerate(
            github.get(&format!("{run}/artifacts?per_page=100")),
            &mut failed_looks,
        )?
        else {
            continue;
        };
        failed_looks = 0;
        absent = missing(&reply, &args.artifacts);
        if absent.is_empty() {
            return Ok(format!(
                "wait-artifacts: the {} artifact(s) are there: {}",
                args.artifacts.len(),
                args.artifacts.join(", ")
            ));
        }
        if !shards.is_empty() && shards.iter().all(|(_, conclusion)| conclusion.is_some()) {
            return Err(Error::Failed(format!(
                "every job named {}* has finished and these artifacts did not come: {}",
                args.job_prefix,
                absent.join(", ")
            )));
        }
    }
    Err(Error::Failed(format!(
        "after {} look(s) at the run {} of {} artifact(s) have not come: {}",
        args.polls,
        absent.len(),
        args.artifacts.len(),
        absent.join(", ")
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
