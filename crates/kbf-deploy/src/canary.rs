//! The canary of `job-service apply`: one job through the job service with kbf's client
//! token, after the switch, checked to complete.
//!
//! The job is the frontend's own kbf image (`executor-kbf-image`), whose entrypoint is the task
//! tool, started with `--version` alone: it needs no grant, no pack and no network, so it shows
//! that the service takes kbf's token and places a job of the executor's shape, that a farm
//! agent pulls the kbf image by its digest, and that the container starts and exits 0. It is
//! given no network (`NETWORK_LEVEL_NONE`), 1 core, 1 GiB, a wall limit of [`WALL_SECS`], and
//! the constraints of the executor's jobs (`linux`, `executor-arch`, containers).
//!
//! **It carries no `kbf-frontend` label.** The executor reads that label to find another
//! frontend's jobs and stops when it finds one; a canary with it would stop the frontend it
//! checks. It carries `kbf-canary=<frontend id>` instead, which the executor does not read.
//!
//! [`judge`] decides from what the service answers: a job that `SUCCEEDED` passes; one that
//! ended any other way, or that the service does not know, fails, naming how; a live one is
//! still pending. [`run`] submits the job and asks after it once per poll, with the pause
//! between asks injected, and cancels a job that has not ended after the last ask.

use std::fmt::Write as _;

use js_proto::v1::{Constraints, ExecMode, Isolation, Job, JobSpec, JobState, Needs, NetworkLevel};
use kbf_job_client::{Client, Error};
use tonic::Code;

/// The label the canary job carries, with the frontend id as its value.
pub(crate) const LABEL: &str = "kbf-canary";

/// The canary's wall limit, in seconds: the task tool's `--version` takes well under one.
pub(crate) const WALL_SECS: u64 = 300;

/// What the canary is made of, from the frontend's `kbf.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Canary {
    /// The job's id: unique per run, so the service makes a new job and does not answer
    /// from an earlier canary's.
    pub(crate) id: String,
    /// The kbf image, `<repository>@sha256:<digest>`, without a `docker://` prefix.
    pub(crate) image: String,
    /// `executor-arch`: the hosts the executor's jobs go to.
    pub(crate) arch: String,
    pub(crate) frontend: String,
}

impl Canary {
    /// The canary job.
    pub(crate) fn spec(&self) -> JobSpec {
        JobSpec {
            job_id: self.id.clone(),
            image: self.image.clone(),
            command: String::new(),
            args: vec!["--version".to_owned()],
            needs: Some(Needs {
                cpu_millis: 1000,
                memory_bytes: 1 << 30,
                disk_bytes: Some(1 << 30),
                ..Needs::default()
            }),
            constraints: Some(Constraints {
                os: "linux".to_owned(),
                arch: self.arch.clone(),
                exec: ExecMode::Container.into(),
                // No ISA level, no accelerator, and confinement left unspecified (contained):
                // the request's bytes stay those an older job service also reads.
                isa_level: String::new(),
                accelerator: None,
                isolation: Isolation::Unspecified.into(),
            }),
            wall_limit_seconds: Some(WALL_SECS),
            network: NetworkLevel::None.into(),
            labels: [(LABEL.to_owned(), self.frontend.clone())]
                .into_iter()
                .collect(),
            ..JobSpec::default()
        }
    }
}

/// What one answer about the canary job says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The job succeeded: the line to print.
    Passed(String),
    /// The job ended another way, or is gone: why.
    Failed(String),
    /// The job is live: its state.
    Pending(&'static str),
}

fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Unspecified => "unspecified",
        JobState::Queued => "queued",
        JobState::Placed => "placed",
        JobState::Starting => "starting",
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Lost => "lost",
        JobState::FailedToStart => "failed to start",
    }
}

/// The verdict on the canary job `id` from the service's answer to `GetJob`: the job, or
/// `None` when the service does not know it.
pub(crate) fn judge(id: &str, job: Option<&Job>) -> Verdict {
    let Some(job) = job else {
        return Verdict::Failed(format!("the job service does not know the canary job {id}"));
    };
    let state = JobState::try_from(job.state).unwrap_or(JobState::Unspecified);
    let host = if job.host_id.is_empty() {
        "no host".to_owned()
    } else {
        format!("host {}", job.host_id)
    };
    match state {
        JobState::Succeeded => Verdict::Passed(format!(
            "pass: the canary job {id} {} on {host}",
            state_name(state)
        )),
        JobState::Queued | JobState::Placed | JobState::Starting | JobState::Running => {
            Verdict::Pending(state_name(state))
        }
        JobState::Failed
        | JobState::Cancelled
        | JobState::Lost
        | JobState::FailedToStart
        | JobState::Unspecified => {
            let mut why = format!("the canary job {id} ended {} on {host}", state_name(state));
            if !job.start_failure_detail.is_empty() {
                let _ = write!(why, ": {}", job.start_failure_detail);
            }
            Verdict::Failed(why)
        }
    }
}

/// The answer to `GetJob` as [`judge`] takes it: a job, none (`NOT_FOUND`), or a failed call.
fn known(answer: Result<Job, Error>) -> Result<Option<Job>, String> {
    match answer {
        Ok(job) => Ok(Some(job)),
        Err(Error::Status {
            code: Code::NotFound,
            ..
        }) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Submit the canary and ask after it until it ends, at most `polls` times, calling `pause`
/// before each ask after the first. A job that is still live after the last ask is
/// cancelled and fails the canary.
pub(crate) async fn run(
    client: &Client,
    canary: &Canary,
    polls: u64,
    pause: &mut dyn FnMut(),
) -> Result<String, String> {
    let id = &canary.id;
    client
        .submit_job(canary.spec())
        .await
        .map_err(|e| format!("the job service did not take the canary job {id}: {e}"))?;
    let mut last = "submitted";
    for ask in 0..polls.max(1) {
        if ask > 0 {
            pause();
        }
        let job = known(client.get_job(id).await)
            .map_err(|e| format!("cannot read the canary job {id}: {e}"))?;
        match judge(id, job.as_ref()) {
            Verdict::Passed(line) => return Ok(line),
            Verdict::Failed(why) => return Err(why),
            Verdict::Pending(state) => last = state,
        }
    }
    let cancelled = match client.cancel_job(id).await {
        Ok(_) => "it was cancelled".to_owned(),
        Err(e) => format!("its cancel failed: {e}"),
    };
    Err(format!(
        "the canary job {id} did not end in time (still {last}); {cancelled}"
    ))
}

#[cfg(test)]
#[path = "canary_tests.rs"]
mod tests;
