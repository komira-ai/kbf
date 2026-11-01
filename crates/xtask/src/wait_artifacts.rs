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
//! **Grace after the last job.** The artifact list can lag behind the job list: the last shard
//! may be seen finished while the artifact it uploaded a second earlier is not listed yet. When
//! every such job has finished and a named artifact is still missing, the wait therefore goes on
//! for `--grace-polls` further looks (at the same interval) before it fails naming what is
//! missing; when the artifacts come in the grace, the line printed says after how many looks.
//!
//! **Artifacts of an earlier attempt.** A re-run of the run keeps the run id and the artifacts
//! of its earlier attempts, so a name that is there is not proof that this attempt's shard made
//! it. An `--artifact` may therefore say which job makes it, `NAME=JOBTEXT`: the job whose name
//! contains `JOBTEXT` (of the jobs reply of `filter=latest`; if a name is listed for more than one
//! attempt, the newest attempt's record is the one). With no job of that text listed it does not
//! count. Whether the job ran in this attempt of the run decides:
//!
//! - A job that did **not** run in this attempt (carried over by "Re-run failed jobs" or "Re-run
//!   this job"; its `run_attempt` is older than the run's, or its `started_at` precedes the
//!   attempt's `run_started_at`) made its artifact in an earlier attempt, which is the artifact to
//!   use: it counts whatever its time is.
//! - A job that **did** run in this attempt counts only when its artifact was created at or after
//!   the job's `started_at` (a queued job has its creation time). Attempt 1's artifacts, which
//!   "Re-run all jobs" and "Re-run failed jobs" leave in place for the shards that run again, are
//!   older than that, so the wait goes on until each such shard has uploaded again.
//!
//! A time is an RFC 3339 string in UTC, which orders as text. An `--artifact` with no `=` counts
//! by its name alone.
//!
//! GitHub is spoken to with `gh api` ([`crate::advisories::Process`]); the job's token needs
//! the `actions: read` permission.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::Error;
use crate::advisories::{Call, Method, Process, Tools};

/// How many looks in a row may fail (GitHub answers an error) before the wait fails with the
/// last error. At the default interval that is about five minutes of errors, after a build of
/// twenty minutes or more.
pub const FAILED_LOOKS: u32 = 20;

/// The arguments of `wait-artifacts`.
#[derive(clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The run whose artifacts and jobs are looked at (on Actions, `GITHUB_RUN_ID`).
    #[arg(long)]
    pub run_id: u64,
    /// The exact name of an artifact to wait for; once for each. `NAME=JOBTEXT` names the job
    /// that makes it too (see the module docs: an artifact of an earlier attempt of a job that
    /// runs again does not count).
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
    /// The further looks after every job has finished, for an artifact that is not listed yet.
    #[arg(long, default_value_t = 8)]
    pub grace_polls: u32,
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

/// A job whose name contains the prefix.
struct Shard<'a> {
    name: &'a str,
    /// `None` while the job has not finished.
    conclusion: Option<&'a str>,
    /// When the job started (an RFC 3339 time in UTC, which orders as text).
    started: Option<&'a str>,
    /// The attempt of the run the record is of; `None` when the reply does not say.
    attempt: Option<u64>,
}

/// The attempt of the run that is being waited in.
#[derive(Default)]
struct Attempt {
    number: Option<u64>,
    /// When the attempt began.
    started: Option<String>,
}

impl Attempt {
    fn of(run: &Value) -> Self {
        Attempt {
            number: run.get("run_attempt").and_then(Value::as_u64),
            started: run
                .get("run_started_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }

    /// Whether the job did not run in this attempt: its record is of an earlier one, or it
    /// started before this one began.
    fn carried_over(&self, shard: &Shard) -> bool {
        let earlier = matches!((shard.attempt, self.number), (Some(a), Some(n)) if a < n);
        let before = matches!((shard.started, &self.started), (Some(s), Some(b)) if s < b.as_str());
        earlier || before
    }
}

/// The jobs whose name contains `prefix`.
/// A name listed for more than one attempt is the record of the newest.
fn shards<'a>(jobs: &'a Value, prefix: &str) -> Vec<Shard<'a>> {
    let mut found: Vec<Shard<'a>> = Vec::new();
    for job in items(jobs, "jobs") {
        let Some(name) = job.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !name.contains(prefix) {
            continue;
        }
        let shard = Shard {
            name,
            conclusion: job.get("conclusion").and_then(Value::as_str),
            started: job.get("started_at").and_then(Value::as_str),
            attempt: job.get("run_attempt").and_then(Value::as_u64),
        };
        match found.iter_mut().find(|s| s.name == name) {
            Some(old) if old.attempt <= shard.attempt => *old = shard,
            Some(_) => {}
            None => found.push(shard),
        }
    }
    found
}

/// The first of the jobs that did not succeed, as `name (conclusion)`.
fn failed(shards: &[Shard]) -> Option<String> {
    shards.iter().find_map(|s| {
        let conclusion = s.conclusion?;
        matches!(conclusion, "failure" | "cancelled" | "timed_out")
            .then(|| format!("{} ({conclusion})", s.name))
    })
}

/// An `--artifact`: its name, and the text of the name of the job that makes it.
fn split(want: &str) -> (&str, Option<&str>) {
    match want.split_once('=') {
        Some((name, job)) => (name, Some(job)),
        None => (want, None),
    }
}

/// Whether `artifact` is the one of the job named by `job`: the job did not run in this attempt,
/// or the artifact was created at or after its start.
fn made_by(artifact: &Value, job: &str, shards: &[Shard], attempt: &Attempt) -> bool {
    let mine: Vec<&Shard> = shards.iter().filter(|s| s.name.contains(job)).collect();
    let created = artifact.get("created_at").and_then(Value::as_str);
    !mine.is_empty()
        && mine.iter().all(|s| {
            attempt.carried_over(s)
                || matches!((s.started, created), (Some(start), Some(made)) if made >= start)
        })
}

/// The names of `wanted` that no live (not expired) artifact of the reply counts for.
fn missing(reply: &Value, wanted: &[String], shards: &[Shard], attempt: &Attempt) -> Vec<String> {
    let live: Vec<&Value> = items(reply, "artifacts")
        .filter(|a| !a.get("expired").and_then(Value::as_bool).unwrap_or(false))
        .collect();
    wanted
        .iter()
        .map(|w| split(w))
        .filter(|(name, job)| {
            !live.iter().any(|a| {
                a.get("name").and_then(Value::as_str) == Some(name)
                    && job.is_none_or(|j| made_by(a, j, shards, attempt))
            })
        })
        .map(|(name, _)| name.to_owned())
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
    let mut absent: Vec<String> = args
        .artifacts
        .iter()
        .map(|a| split(a).0.to_owned())
        .collect();
    let mut failed_looks = 0;
    let mut attempt: Option<Attempt> = None;
    let mut grace = 0;
    for look in 0..args.polls {
        if look > 0 {
            github.pause();
        }
        if attempt.is_none() {
            let Some(reply) = tolerate(github.get(&run), &mut failed_looks)? else {
                continue;
            };
            attempt = Some(Attempt::of(&reply));
        }
        let attempt_now = attempt.as_ref().expect("fetched above");
        let Some(jobs) = tolerate(
            github.get(&format!("{run}/jobs?filter=latest&per_page=100")),
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
        absent = missing(&reply, &args.artifacts, &shards, attempt_now);
        if absent.is_empty() {
            let late = if grace > 0 {
                format!(" (listed {grace} look(s) after the last job finished)")
            } else {
                String::new()
            };
            return Ok(format!(
                "wait-artifacts: the {} artifact(s) are there: {}{late}",
                args.artifacts.len(),
                args.artifacts
                    .iter()
                    .map(|a| split(a).0)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !shards.is_empty() && shards.iter().all(|s| s.conclusion.is_some()) {
            if grace >= args.grace_polls {
                return Err(Error::Failed(format!(
                    "every job named {}* has finished and these artifacts did not come, even {} look(s) later: {}",
                    args.job_prefix,
                    grace,
                    absent.join(", ")
                )));
            }
            grace += 1;
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
