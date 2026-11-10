//! The job service, as the executor calls it: the six calls it makes, every one bounded.
//!
//! The client gives a call its connect timeout and then 30 s, and a pass of the executor makes
//! several calls one after another, with the heartbeats and the self-fence between them. A job
//! service that accepts and never answers would hold a pass for minutes, and the farm hears
//! nothing meanwhile. Each call here gives up after [`Config::call_timeout`], and the answer
//! is the one a lost answer gives ([`Error::Transport`]): the request may have been sent, so
//! [`Error::may_have_applied`] is true and the caller repeats it.
//!
//! [`Config::call_timeout`]: super::Config::call_timeout

use std::future::Future;
use std::time::Duration;

use js_proto::v1::{Job, JobResult, JobSpec, ListJobsRequest};
use kbf_job_client::{Broken, Client, Error};

use crate::executor::output::Tail;

/// The client of the job service, with a limit on each call.
#[derive(Clone, Copy)]
pub(crate) struct Api<'a> {
    pub(super) client: &'a Client,
    pub(super) limit: Duration,
}

impl Api<'_> {
    async fn within<T>(&self, call: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        let limit = self.limit;
        tokio::time::timeout(limit, call)
            .await
            .unwrap_or(Err(Error::Transport(Broken::Deadline(limit))))
    }

    /// `SubmitJob`: idempotent on the spec's job id.
    pub(crate) async fn submit_job(&self, spec: JobSpec) -> Result<Job, Error> {
        self.within(self.client.submit_job(spec)).await
    }

    /// `GetJob`.
    pub(crate) async fn get_job(&self, id: &str) -> Result<Job, Error> {
        self.within(self.client.get_job(id)).await
    }

    /// `ListJobs`, every page: for the adoption of an earlier process's jobs and the rival
    /// check only.
    pub(crate) async fn list_all_jobs(&self, filter: ListJobsRequest) -> Result<Vec<Job>, Error> {
        self.within(self.client.list_all_jobs(filter)).await
    }

    /// `CancelJob`: idempotent on the job.
    pub(crate) async fn cancel_job(&self, id: &str) -> Result<Job, Error> {
        self.within(self.client.cancel_job(id)).await
    }

    /// `GetResult` of an ended job.
    pub(crate) async fn get_result(&self, id: &str) -> Result<JobResult, Error> {
        self.within(self.client.get_result(id)).await
    }

    /// The tails of the output of job `id`, by `GetLogs` without a stream: for a job that did
    /// not end well and whose result holds no output ([`output`](super::output)). The service
    /// answers the tails of the job's result (each stream's last 8 KiB, the most of it the
    /// executor keeps), and none for a job with no result. `GetLogs` also reads a stream from an
    /// offset, from the service's log store; this is the one place that would ask for the last
    /// [`OUTPUT_CAP`](super::output::OUTPUT_CAP) bytes by it, and does not yet.
    pub(super) async fn logs_tail(&self, id: &str) -> Result<Tail, Error> {
        let logs = self.within(self.client.get_logs(id)).await?;
        Ok(Tail::of_logs(logs))
    }
}

/// Whether the service answered about the job: it took the call and said what it did with
/// it. A call never sent, an answer lost, and a status that says the service will not serve
/// this client at all (`UNAUTHENTICATED`, `PERMISSION_DENIED`, `RESOURCE_EXHAUSTED`,
/// `UNIMPLEMENTED`, ...) are not: a revoked token is a service that cannot be reached, for the
/// self-fence and for a cancel, not one that answers.
pub(crate) fn answered<T>(answer: &Result<T, Error>) -> bool {
    use tonic::Code;
    match answer {
        Ok(_) => true,
        Err(Error::Refused(_) | Error::TooMany(_) | Error::Stuck(_)) => true,
        Err(Error::Status { code, .. }) => matches!(
            code,
            Code::NotFound | Code::FailedPrecondition | Code::InvalidArgument | Code::AlreadyExists
        ),
        Err(Error::Unreachable(_) | Error::Transport(_) | Error::Decode(_)) => false,
    }
}

/// Whether the service said it has no such job.
pub(crate) fn not_found(error: &Error) -> bool {
    matches!(
        error,
        Error::Status {
            code: tonic::Code::NotFound,
            ..
        }
    )
}

/// Logs, once and in one place, that a call to the job service failed and the caller goes on.
pub(crate) fn note<T>(what: &str, answer: &Result<T, Error>) {
    if let Err(error) = answer {
        tracing::warn!(%error, "{what} failed");
    }
}
