//! The lease server, as the executor calls it: every call bounded.
//!
//! The client times a request out only after 30 s, and a pass of the executor makes several
//! calls one after another, with the heartbeats and the self-fence between them. A lease
//! server that accepts and never answers would hold a pass for minutes, and the farm hears
//! nothing meanwhile. Each call here gives up after [`Config::call_timeout`], and the answer
//! is the one a broken connection gives: the request may have been sent.
//!
//! [`Config::call_timeout`]: super::Config::call_timeout

use std::future::Future;
use std::time::Duration;

use kbf_lease_client::{
    AcquireRequest, Acquired, Client, Error, Host, Lease, LeaseId, ReleaseState, Renewed, Run,
    RunAccepted, RunId, RunSpec,
};

/// The client of the lease server, with a limit on each call.
#[derive(Clone, Copy)]
pub(crate) struct Api<'a> {
    pub(super) client: &'a Client,
    pub(super) limit: Duration,
}

/// What the lease server's answer is when it gave none in time.
#[derive(Debug, thiserror::Error)]
#[error("no answer from the lease server within {0:?}")]
struct NoAnswer(Duration);

impl Api<'_> {
    async fn within<T>(&self, call: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        match tokio::time::timeout(self.limit, call).await {
            Ok(answer) => answer,
            Err(_) => Err(Error::Transport(Box::new(NoAnswer(self.limit)))),
        }
    }

    pub(crate) async fn acquire(&self, request: &AcquireRequest) -> Result<Acquired, Error> {
        self.within(self.client.acquire(request)).await
    }

    pub(crate) async fn renew(
        &self,
        lease: LeaseId,
        ttl: Option<Duration>,
    ) -> Result<Renewed, Error> {
        self.within(self.client.renew(lease, ttl)).await
    }

    pub(crate) async fn release(&self, lease: LeaseId) -> Result<ReleaseState, Error> {
        self.within(self.client.release(lease)).await
    }

    pub(crate) async fn leases(&self) -> Result<Vec<Lease>, Error> {
        self.within(self.client.leases()).await
    }

    pub(crate) async fn leases_labelled(
        &self,
        key: &str,
        value: &str,
    ) -> Result<Vec<Lease>, Error> {
        self.within(self.client.leases_labelled(key, value)).await
    }

    pub(crate) async fn hosts(&self) -> Result<Vec<Host>, Error> {
        self.within(self.client.hosts()).await
    }

    pub(crate) async fn queue_run(
        &self,
        lease: LeaseId,
        run: &RunSpec,
    ) -> Result<RunAccepted, Error> {
        self.within(self.client.queue_run(lease, run)).await
    }

    pub(crate) async fn runs(&self, lease: LeaseId) -> Result<Vec<Run>, Error> {
        self.within(self.client.runs(lease)).await
    }

    pub(crate) async fn stop_run(&self, lease: LeaseId, run: &RunId) -> Result<RunAccepted, Error> {
        self.within(self.client.stop_run(lease, run)).await
    }
}
