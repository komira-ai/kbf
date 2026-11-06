//! The pool of virtual workers, and what a restarted frontend does with the jobs an earlier
//! process left.
//!
//! The executor registers [`Config::hosts`] virtual workers once it has taken up the earlier
//! jobs ([`adopt_jobs`]), each of the declared shape, and keeps them for as long as it runs:
//! the farm places on workers, and the job service decides where each job runs.
//!
//! [`Config::hosts`]: super::Config::hosts

use std::collections::BTreeSet;

use js_proto::v1::{Job, JobState, ListJobsRequest};
use tokio::time::Instant;

use crate::executor::api::{answered, note};
use crate::executor::host::HostWorker;
use crate::executor::submit::{FRONTEND_LABEL, INSTANCE_LABEL};
use crate::executor::{Ctx, twin_message};

/// The states of a job that has not ended.
pub(super) const LIVE: [JobState; 4] = [
    JobState::Queued,
    JobState::Placed,
    JobState::Starting,
    JobState::Running,
];

/// A listing of the live jobs with `labels`.
pub(super) fn live_jobs(labels: &[(&str, &str)]) -> ListJobsRequest {
    ListJobsRequest {
        states: LIVE.iter().map(|state| i32::from(*state)).collect(),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        ..ListJobsRequest::default()
    }
}

/// The workers of this executor, registered with the farm.
pub(super) fn workers(ctx: &Ctx<'_>, now: Instant) -> Vec<HostWorker> {
    (0..ctx.cfg.hosts)
        .map(|n| HostWorker::start(ctx, n, now))
        .collect()
}

/// Whether the jobs an earlier process left under this frontend's id have been taken up.
pub(super) enum Adoption {
    /// Not yet looked at.
    Unread,
    /// Live jobs of another process under this id were seen: a process that is alive makes
    /// new ones, so they are looked at again at `until`. `cancelled` are those of them whose
    /// cancel the service has answered.
    Watching {
        seen: BTreeSet<String>,
        until: Instant,
        cancelled: BTreeSet<String>,
    },
    /// Taken up.
    Done,
}

impl Adoption {
    pub(super) fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }
}

/// What taking up the earlier jobs came to.
pub(super) enum Adopted {
    /// Taken up: the workers may be registered and jobs submitted.
    Done,
    /// Jobs of another process under this id are being watched for a new one, or their
    /// cancels are not all answered yet.
    Watching,
    /// A new one was made in the watch: that process is alive.
    Twin(String),
    /// The job service could not be asked.
    Unread,
}

/// Whether `job` was made by another process of this frontend's id.
fn stranger(ctx: &Ctx<'_>, job: &Job) -> bool {
    let labels = job.spec.as_ref().map(|spec| &spec.labels);
    labels.and_then(|labels| labels.get(INSTANCE_LABEL)) != Some(&ctx.cfg.instance)
}

/// Takes up the jobs this frontend's id left before this process started: once, before the
/// first submit, the live jobs labelled with the frontend's id are listed, and every one made
/// by another process is cancelled (nothing of the farm maps to it any more) and counted.
///
/// A process under the same frontend id may be alive (two frontends given one config), and
/// its jobs would be cancelled. So they are seen, and seen again after the longest a live
/// executor goes without submitting what the farm placed on it ([`Config::adopt_watch`]); if a
/// job of another process appeared in between, that process is alive and the caller stops.
/// Otherwise the jobs are cancelled. Nothing is submitted until the list was read, and a list
/// that could not be read is asked for again on the next pass.
///
/// [`Config::adopt_watch`]: super::Config::adopt_watch
pub(super) async fn adopt_jobs(ctx: &Ctx<'_>, now: Instant, adoption: &mut Adoption) -> Adopted {
    if let Adoption::Watching { until, .. } = adoption
        && now < *until
    {
        return Adopted::Watching;
    }
    let filter = live_jobs(&[(FRONTEND_LABEL, &ctx.cfg.frontend)]);
    let listed = ctx.api().list_all_jobs(filter).await;
    note("listing the jobs of an earlier process", &listed);
    let Ok(own) = listed else {
        return Adopted::Unread;
    };
    let strangers: Vec<&Job> = own.iter().filter(|job| stranger(ctx, job)).collect();
    match adoption {
        Adoption::Watching {
            seen, cancelled, ..
        } => {
            let new = strangers
                .iter()
                .any(|job| !seen.contains(&job.id) && !cancelled.contains(&job.id));
            if new {
                return Adopted::Twin(twin_message(&ctx.cfg.frontend));
            }
            let left: Vec<&Job> = strangers
                .iter()
                .filter(|job| !cancelled.contains(&job.id))
                .copied()
                .collect();
            let mut all = true;
            for job in left {
                let answer = ctx.api().cancel_job(&job.id).await;
                note("cancelling a job of an earlier process", &answer);
                if answer.is_ok() {
                    ctx.deps.metrics.job_adopted_cancelled();
                }
                if answered(&answer) {
                    cancelled.insert(job.id.clone());
                }
                all &= answered(&answer);
            }
            match all {
                true => Adopted::Done,
                false => Adopted::Watching,
            }
        }
        _ if strangers.is_empty() => Adopted::Done,
        _ => {
            *adoption = Adoption::Watching {
                seen: strangers.iter().map(|job| job.id.clone()).collect(),
                until: now + ctx.cfg.adopt_watch(),
                cancelled: BTreeSet::new(),
            };
            Adopted::Watching
        }
    }
}
