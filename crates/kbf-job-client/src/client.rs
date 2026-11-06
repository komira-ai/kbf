//! The client: one unary gRPC call per method, over one tonic channel.
//!
//! The channel connects lazily, on the first call, through [`Dial`]; building a client
//! touches no network. It holds one HTTP/2 connection and multiplexes calls on it, and makes
//! a new one when that breaks. Clones share the channel, so any number of calls may be in
//! flight at once. The client never retries: what a caller does after an error is
//! [`Error::may_have_applied`]'s to say.

use std::collections::BTreeSet;
use std::time::Duration;

use http::uri::PathAndQuery;
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, GetJobRequest, GetJobResponse, GetLogsRequest,
    GetLogsResponse, GetResultRequest, GetResultResponse, Job, JobResult, JobSpec, ListJobsRequest,
    ListJobsResponse, SubmitJobRequest, SubmitJobResponse,
};
use prost::Message;
use tonic::Status;
use tonic::client::Grpc;
use tonic::transport::{Channel, Endpoint};

use crate::codec::Lenient;
use crate::config::{Config, Origin, Token};
use crate::dial::Dial;
use crate::error::{Broken, ConfigError, Error, Stuck, Undecodable, Unsent, from_status};
use crate::id;

// The path of each method of `jobscheduler.v1.JobService` the client calls.
const SUBMIT_JOB: &str = "/jobscheduler.v1.JobService/SubmitJob";
const GET_JOB: &str = "/jobscheduler.v1.JobService/GetJob";
const LIST_JOBS: &str = "/jobscheduler.v1.JobService/ListJobs";
const CANCEL_JOB: &str = "/jobscheduler.v1.JobService/CancelJob";
const GET_RESULT: &str = "/jobscheduler.v1.JobService/GetResult";
const GET_LOGS: &str = "/jobscheduler.v1.JobService/GetLogs";

/// A client of one job service. Cheap to clone; the clones share one channel, so any number
/// of calls may be in flight at once.
#[derive(Clone)]
pub struct Client {
    grpc: Grpc<Channel>,
    origin: Origin,
    token: Token,
    /// The most a call may take: connecting, then the exchange (see [`Config::with_timeouts`]).
    deadline: Duration,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("origin", &self.origin.authority())
            .field("tls", &self.origin.is_tls())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// The most bytes a request or an answer may hold: the size the service's server takes
    /// by default. A bigger request is not sent; a bigger answer is an error, not a value.
    pub const MAX_MESSAGE: usize = 4 << 20;

    /// The most jobs [`Client::list_all_jobs`] collects before it stops with
    /// [`Error::TooMany`].
    pub const MAX_LISTED: usize = 10_000;

    /// The most empty pages in a row, each with a next page token, that
    /// [`Client::list_all_jobs`] follows before it stops with [`Error::Stuck`].
    pub const MAX_EMPTY_PAGES: usize = 100;

    /// A client for `config`. Nothing is dialled until the first call.
    ///
    /// # Errors
    /// An `https` origin has no CA, or its CA holds no usable certificate, or an `http`
    /// origin has a CA.
    ///
    /// # Panics
    /// Outside a Tokio runtime: the channel starts its worker task on the runtime.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let Config {
            origin,
            token,
            ca_pem,
            connect_timeout,
            request_timeout,
        } = config;
        let dial = Dial::new(&origin, ca_pem.as_deref(), connect_timeout)?;
        let endpoint = Endpoint::from(origin.endpoint().clone()).origin(origin.uri().clone());
        let channel = endpoint.connect_with_connector_lazy(dial);
        let grpc = Grpc::new(channel)
            .max_decoding_message_size(Self::MAX_MESSAGE)
            .max_encoding_message_size(Self::MAX_MESSAGE);
        Ok(Self {
            grpc,
            origin,
            token,
            deadline: connect_timeout.saturating_add(request_timeout),
        })
    }

    /// Submits a job (`SubmitJob`) and answers the job as recorded: queued for a new one, or
    /// the job already made, in whatever state it is now, for a repeated one.
    ///
    /// The caller sets `spec.job_id`, and keeps it: the id is what makes the submit
    /// idempotent. The client never makes one up, because an id the caller did not store
    /// could not be sent again. After an error for which [`Error::may_have_applied`] is
    /// true, submitting the same spec with the same id again is safe: it answers the job
    /// the first submit made, and never runs the work twice.
    ///
    /// # Errors
    /// [`Error::Unreachable`] with a [`BadId`](crate::BadId) for an id the service would not
    /// take, empty included, with nothing sent. [`Error::Refused`] with `job-id-conflict`
    /// (the id names a job that differs in more than its priority), `no-capable-host`,
    /// `wall-time-not-positive` or `wall-time-above-ceiling`; `INVALID_ARGUMENT` for a job
    /// the service rejects otherwise; and the transport errors.
    pub async fn submit_job(&self, spec: JobSpec) -> Result<Job, Error> {
        checked(&spec.job_id)?;
        let request = SubmitJobRequest { spec: Some(spec) };
        let answer: SubmitJobResponse = self.call(SUBMIT_JOB, request).await?;
        answer.job.ok_or(Error::Decode(Undecodable::Missing("job")))
    }

    /// Reads a job (`GetJob`): its state, its host once placed, its times and its host's
    /// silence. Changes nothing, so it may be repeated after any error.
    ///
    /// # Errors
    /// `NOT_FOUND` for a job the service does not have or whose retention window has
    /// passed, and the transport errors.
    pub async fn get_job(&self, id: &str) -> Result<Job, Error> {
        checked(id)?;
        let request = GetJobRequest {
            job_id: id.to_owned(),
        };
        let answer: GetJobResponse = self.call(GET_JOB, request).await?;
        answer.job.ok_or(Error::Decode(Undecodable::Missing("job")))
    }

    /// One page of the jobs that match `filter` (`ListJobs`): the request's states, labels
    /// and queued times narrow it, its page size and token pick the page, and the answer's
    /// `next_page_token` reads the next one (empty on the last). Changes nothing.
    ///
    /// # Errors
    /// `INVALID_ARGUMENT` for a page token the service did not write, and the transport
    /// errors.
    pub async fn list_jobs(&self, filter: ListJobsRequest) -> Result<ListJobsResponse, Error> {
        self.call(LIST_JOBS, filter).await
    }

    /// Every job that matches `filter`, following the page tokens from the filter's own
    /// token to the last page; each page is asked for with the filter's states, labels,
    /// times and page size. It stops with [`Error::TooMany`] once more than
    /// [`Client::MAX_LISTED`] jobs have come, rather than hold an unbounded listing or cut
    /// it short without saying so, and with [`Error::Stuck`] when the service gives a page
    /// token a second time or [`Client::MAX_EMPTY_PAGES`] empty pages in a row, rather than
    /// ask for ever.
    ///
    /// # Errors
    /// [`Error::TooMany`], [`Error::Stuck`], and as [`Client::list_jobs`] for each page.
    pub async fn list_all_jobs(&self, filter: ListJobsRequest) -> Result<Vec<Job>, Error> {
        let mut page = filter;
        let mut jobs = Vec::new();
        let mut tokens = BTreeSet::new();
        let mut empty = 0;
        loop {
            let answer = self.list_jobs(page.clone()).await?;
            empty = if answer.jobs.is_empty() { empty + 1 } else { 0 };
            jobs.extend(answer.jobs);
            if jobs.len() > Self::MAX_LISTED {
                return Err(Error::TooMany(Self::MAX_LISTED));
            }
            let next = answer.next_page_token;
            if next.is_empty() {
                return Ok(jobs);
            }
            if empty >= Self::MAX_EMPTY_PAGES {
                return Err(Error::Stuck(Stuck::EmptyPages(empty)));
            }
            if !tokens.insert(next.clone()) {
                return Err(Error::Stuck(Stuck::RepeatedToken(next)));
            }
            page.page_token = next;
        }
    }

    /// Cancels a job (`CancelJob`) and answers the job after the cancel was recorded:
    /// cancelled if it was queued; with `cancel_requested` set and its state unchanged if it
    /// was placed, starting or running (it is cancelled when its host reports it stopped);
    /// unchanged if it had ended. Cancelling twice is the same as once, so after an error
    /// for which [`Error::may_have_applied`] is true the caller cancels again.
    ///
    /// # Errors
    /// `NOT_FOUND` for a job the service does not have, and the transport errors.
    pub async fn cancel_job(&self, id: &str) -> Result<Job, Error> {
        checked(id)?;
        let request = CancelJobRequest {
            job_id: id.to_owned(),
        };
        let answer: CancelJobResponse = self.call(CANCEL_JOB, request).await?;
        answer.job.ok_or(Error::Decode(Undecodable::Missing("job")))
    }

    /// The result of an ended job (`GetResult`), for its retention window. Changes nothing.
    ///
    /// # Errors
    /// `FAILED_PRECONDITION` for a job that has not ended, `NOT_FOUND` for one the service
    /// does not have or whose window has passed, and the transport errors.
    pub async fn get_result(&self, id: &str) -> Result<JobResult, Error> {
        checked(id)?;
        let request = GetResultRequest {
            job_id: id.to_owned(),
        };
        let answer: GetResultResponse = self.call(GET_RESULT, request).await?;
        answer
            .result
            .ok_or(Error::Decode(Undecodable::Missing("result")))
    }

    /// The log tail held so far (`GetLogs`): of a running job too, and of an ended one for
    /// its retention window. Each stream's tail is at most 8 KiB. Changes nothing.
    ///
    /// # Errors
    /// `NOT_FOUND` for a job the service does not have or whose window has passed, and the
    /// transport errors.
    pub async fn get_logs(&self, id: &str) -> Result<GetLogsResponse, Error> {
        checked(id)?;
        let request = GetLogsRequest {
            job_id: id.to_owned(),
        };
        self.call(GET_LOGS, request).await
    }

    /// One call of the method at `path`: the request checked against the size cap, the token
    /// put in its metadata, the whole call under the connect and request timeouts together,
    /// and the answer decoded. A connection not made within the connect timeout fails in the
    /// connector, as nothing sent, before this deadline can pass.
    async fn call<Req, Resp>(&self, path: &'static str, message: Req) -> Result<Resp, Error>
    where
        Req: Message + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
    {
        let len = message.encoded_len();
        if len > Self::MAX_MESSAGE {
            let cap = Self::MAX_MESSAGE;
            return Err(Error::Unreachable(Unsent::TooLarge { len, cap }));
        }
        let mut request = tonic::Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", self.token.value().clone());
        let path = PathAndQuery::from_static(path);
        let mut grpc = self.grpc.clone();
        let exchange = async move {
            grpc.ready()
                .await
                .map_err(|cause| Status::from_error(cause.into()))?;
            grpc.unary(request, path, Lenient::<Req, Resp>::new()).await
        };
        match tokio::time::timeout(self.deadline, exchange).await {
            Err(_) => Err(Error::Transport(Broken::Deadline(self.deadline))),
            Ok(Err(status)) => Err(from_status(status)),
            Ok(Ok(answer)) => answer
                .into_inner()
                .map_err(|cause| Error::Decode(Undecodable::Bytes(cause))),
        }
    }
}

/// `id`, checked as the service would check it, before anything is sent.
fn checked(id: &str) -> Result<(), Error> {
    id::check(id).map_err(|bad| Error::Unreachable(Unsent::BadId(bad)))
}
