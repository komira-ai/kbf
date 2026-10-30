//! Several endpoints for one upstream: a [`Backend`] that reads from the first of a list of
//! equivalent endpoints that answers, in an order that depends on the key.
//!
//! A build node's `[cold_store]` may list up to 16 endpoints (storage proxies over the same
//! store, so any one serves any key). [`Endpoints`] is the upstream of the local cache in that
//! case: it takes the place of the one S3 backend and the cache does not know the difference.
//!
//! # Order
//!
//! For each call the endpoints are ranked by rendezvous hashing of the key (see the `rank` module) and
//! tried in that order. The same key goes to the same endpoint while it is usable, so the
//! ranged reads of one pack meet one proxy's connections and page cache, and the keys spread
//! over the endpoints evenly. A listing is ranked by its prefix.
//!
//! # What is a failure
//!
//! An [`Error::Unavailable`] is: the S3 client reports a connection that could not be made, a
//! request that timed out, a 408 or 429, and a 5xx but 501 and 507 that way. The call goes on to
//! the next endpoint, and the endpoint that failed is skipped for
//! [`EndpointsConfig::unready_backoff`], so a dead endpoint is not asked again for every key. Any
//! other answer is an answer and goes back to the caller as it is: a 404 is authoritative (the
//! endpoints share the store, so a miss is a miss), and so are the refusals of the request
//! itself (403, 412, 413).
//!
//! An endpoint is also skipped while it is known not to be ready: [`Endpoints::poll`] asks
//! every endpoint's `/readyz` through the [`Probe`] and records the answer, and
//! [`Endpoints::watch`] does so every [`EndpointsConfig::readyz_interval`]. A draining storage
//! proxy answers 503 there for as long as it keeps serving, so that callers move away first.
//! Both times come from injected parts (the [`Clock`] and the probe), so the tests move them by
//! hand.
//!
//! Every endpoint starts ready. A call that finds every endpoint failed, not ready or backed off
//! is [`Error::Unavailable`]; the cache then still serves the objects it holds, and its
//! `/readyz` says 503 until an endpoint is usable again ([`Endpoints::is_ready`]).
//!
//! # Writes
//!
//! This release is read-only, as the cache is: the three write calls are
//! [`Error::Unsupported`]. A write-through put needs its own rules (retry on another endpoint
//! only before a byte was accepted or from the staged copy, and a 412 on a retry is the first
//! attempt's success), so it is not `Endpoints::across` with a different closure; it will
//! add its own method beside the reads and use the same ranking and state.
//!
//! # Metrics
//!
//! [`Endpoints::metrics`] is added to the cache's `/metricz`:
//! `tierstore_cache_upstream_failovers_total{reason}` counts the endpoints a call went past
//! before one answered (`request_failed`, `not_ready`, `backoff`),
//! `tierstore_cache_upstream_unavailable_total` the calls that found none to answer, and
//! `tierstore_cache_upstream_endpoint_ready{endpoint}` is 1 for an endpoint a call would try now.

mod probe;
mod rank;

#[cfg(test)]
mod failover_tests;
#[cfg(test)]
mod front_tests;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod rank_tests;
#[cfg(test)]
mod s3_tests;
#[cfg(test)]
pub(in crate::server) mod testing;

use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use crate::{Backend, ByteRange, Clock, Error, Listed, Meta, OpenUpload, UploadsAfter};

pub(super) use probe::{HttpProbe, Probe, authority};
use rank::rank;

/// The most endpoints a list may hold.
pub(super) const MAX_ENDPOINTS: usize = 16;

/// How long an endpoint that failed is skipped, unless the file says otherwise.
pub(super) const DEFAULT_UNREADY_BACKOFF: Duration = Duration::from_secs(5);

/// How often the poller asks every endpoint's `/readyz`, unless the file says otherwise.
pub(super) const DEFAULT_READYZ_INTERVAL: Duration = Duration::from_secs(2);

/// The text of the error of a call that found no endpoint to try.
const NONE_READY: &str = "no upstream endpoint is ready";

/// The timing of the endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EndpointsConfig {
    /// How long an endpoint that failed a request is skipped.
    pub unready_backoff: Duration,
    /// The time between two polls of every endpoint's `/readyz`.
    pub readyz_interval: Duration,
}

impl Default for EndpointsConfig {
    fn default() -> Self {
        Self {
            unready_backoff: DEFAULT_UNREADY_BACKOFF,
            readyz_interval: DEFAULT_READYZ_INTERVAL,
        }
    }
}

/// Why a call went past an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Passed {
    /// The call to it failed.
    RequestFailed,
    /// Its `/readyz` did not say 200 the last time it was asked.
    NotReady,
    /// An earlier call to it failed less than `unready_backoff` ago.
    Backoff,
}

/// The label of each [`Passed`], in the order of the enum.
const PASSED: [&str; 3] = ["request_failed", "not_ready", "backoff"];

/// One endpoint and what is known of it.
struct Member {
    /// The URL as the file gives it: the name the ranking hashes and the metric shows.
    name: String,
    backend: Arc<dyn Backend>,
    /// What the last poll of `/readyz` said.
    ready: AtomicBool,
    /// The time, by the clock, until which a call skips the endpoint; zero for no backoff.
    skip_until: AtomicU64,
}

impl Member {
    /// Why a call at `now` goes past this endpoint without trying it, if it does.
    fn skipped(&self, now: u64) -> Option<Passed> {
        if !self.ready.load(Ordering::SeqCst) {
            return Some(Passed::NotReady);
        }
        (now < self.skip_until.load(Ordering::SeqCst)).then_some(Passed::Backoff)
    }
}

/// A list of equivalent endpoints, read as one [`Backend`].
pub(super) struct Endpoints {
    members: Vec<Member>,
    /// The names of the members, in their order, for the ranking.
    names: Vec<String>,
    config: EndpointsConfig,
    clock: Arc<dyn Clock>,
    probe: Arc<dyn Probe>,
    /// The endpoints calls went past, by [`Passed`], once a later endpoint answered.
    failovers: [AtomicU64; PASSED.len()],
    /// The calls that found no endpoint to answer.
    unavailable: AtomicU64,
}

impl Endpoints {
    /// The endpoints `members` (a name and a backend each; the name is the URL), all ready.
    /// The caller has checked that there are 1 to [`MAX_ENDPOINTS`] and that the names differ.
    pub(super) fn new(
        members: Vec<(String, Arc<dyn Backend>)>,
        config: EndpointsConfig,
        clock: Arc<dyn Clock>,
        probe: Arc<dyn Probe>,
    ) -> Self {
        let names = members.iter().map(|(name, _)| name.clone()).collect();
        let members = members
            .into_iter()
            .map(|(name, backend)| Member {
                name,
                backend,
                ready: AtomicBool::new(true),
                skip_until: AtomicU64::new(0),
            })
            .collect();
        Self {
            members,
            names,
            config,
            clock,
            probe,
            failovers: Default::default(),
            unavailable: AtomicU64::new(0),
        }
    }

    /// Whether a call now would try at least one endpoint: one that is ready and is not
    /// backed off. The cache's `/readyz` says this.
    pub(super) fn is_ready(&self) -> bool {
        let now = self.clock.now();
        self.members
            .iter()
            .any(|member| member.skipped(now).is_none())
    }

    /// Asks every endpoint's `/readyz` at once and records the answers.
    pub(super) async fn poll(&self) {
        let asking = self.members.iter().map(|member| async {
            let ready = self.probe.ready(&member.name).await;
            member.ready.store(ready, Ordering::SeqCst);
        });
        futures::future::join_all(asking).await;
    }

    /// Polls every [`EndpointsConfig::readyz_interval`] until `halt` is notified.
    pub(super) async fn watch(&self, halt: Arc<Notify>) {
        loop {
            tokio::select! {
                biased;
                () = halt.notified() => break,
                () = tokio::time::sleep(self.config.readyz_interval) => {}
            }
            self.poll().await;
        }
    }

    /// The text added to `/metricz`.
    pub(super) fn metrics(&self) -> String {
        let mut text = String::new();
        for (label, count) in PASSED.iter().zip(&self.failovers) {
            let count = count.load(Ordering::Relaxed);
            let _ = writeln!(
                text,
                "tierstore_cache_upstream_failovers_total{{reason=\"{label}\"}} {count}"
            );
        }
        let none = self.unavailable.load(Ordering::Relaxed);
        let _ = writeln!(text, "tierstore_cache_upstream_unavailable_total {none}");
        let now = self.clock.now();
        for member in &self.members {
            let usable = u8::from(member.skipped(now).is_none());
            let name = &member.name;
            let _ = writeln!(
                text,
                "tierstore_cache_upstream_endpoint_ready{{endpoint=\"{name}\"}} {usable}"
            );
        }
        text
    }

    /// Runs `call` on the endpoints in the order of `key`, going past the ones that are skipped
    /// and the ones whose call is [`Error::Unavailable`], and gives the first other result.
    /// A call that passed endpoints and was then answered counts one failover for each; one that
    /// found none to answer counts one unavailable and gives the last failure, or
    /// [`Error::Unavailable`] when every endpoint was skipped.
    async fn across<T, Fut>(
        &self,
        key: &str,
        call: impl Fn(Arc<dyn Backend>) -> Fut,
    ) -> Result<T, Error>
    where
        Fut: Future<Output = Result<T, Error>>,
    {
        let now = self.clock.now();
        let mut passed = Vec::new();
        let mut failure = None;
        for place in rank(&self.names, key) {
            let member = &self.members[place];
            if let Some(why) = member.skipped(now) {
                passed.push(why);
                continue;
            }
            match call(Arc::clone(&member.backend)).await {
                Err(error) if error.is_unavailable() => {
                    let backoff = self.config.unready_backoff.as_millis();
                    let until = self.clock.now().saturating_add(backoff as u64);
                    member.skip_until.store(until, Ordering::SeqCst);
                    passed.push(Passed::RequestFailed);
                    failure = Some(error);
                }
                answer => {
                    for why in passed {
                        let _ = self.failovers[why as usize].fetch_add(1, Ordering::Relaxed);
                    }
                    return answer;
                }
            }
        }
        let _ = self.unavailable.fetch_add(1, Ordering::Relaxed);
        Err(failure.unwrap_or_else(|| Error::Unavailable(NONE_READY.to_owned())))
    }

    /// The failovers counted for `why`.
    #[cfg(test)]
    pub(super) fn failovers(&self, why: Passed) -> u64 {
        self.failovers[why as usize].load(Ordering::Relaxed)
    }

    /// The calls that found no endpoint to answer.
    #[cfg(test)]
    pub(super) fn unavailable(&self) -> u64 {
        self.unavailable.load(Ordering::Relaxed)
    }
}

/// The text of the refusal of a write.
const READ_ONLY: &str = "the upstream endpoints are read through; writes are not supported";

#[async_trait]
impl Backend for Endpoints {
    async fn put(&self, _key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::Unsupported(READ_ONLY.to_owned()))
    }

    async fn put_if_match(&self, _key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::Unsupported(READ_ONLY.to_owned()))
    }

    async fn delete(&self, _key: &str) -> Result<(), Error> {
        Err(Error::Unsupported(READ_ONLY.to_owned()))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let reading = |backend: Arc<dyn Backend>| async move { backend.get(key, range).await };
        self.across(key, reading).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let asking = |backend: Arc<dyn Backend>| async move { backend.head(key).await };
        self.across(key, asking).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let listing = |backend: Arc<dyn Backend>| async move { backend.list(prefix).await };
        self.across(prefix, listing).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let paging = |backend: Arc<dyn Backend>| async move {
            backend.list_page(prefix, start_after, max_keys).await
        };
        self.across(prefix, paging).await
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let listing = |backend: Arc<dyn Backend>| async move {
            backend.list_uploads(prefix, after, max_uploads).await
        };
        self.across(prefix, listing).await
    }
}
