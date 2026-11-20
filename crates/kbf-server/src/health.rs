//! `GET /healthz` and `GET /readyz` on the operator API listener, for a front's or a
//! proxy's health check (`docs/api.md`).
//!
//! - `/healthz`: the process is alive and its API answers. It reads nothing and
//!   always answers 200 while the server runs, while it stops too.
//! - `/startedz`: the process has been ready once. 503, with `/readyz`'s failing checks,
//!   until `/readyz`'s checks first all pass; 200 from then on, while it stops and while a
//!   dependency is down too. It is the check a container orchestrator that starts the new
//!   process beside the old one asks before it stops the old one (Swarm's health check with
//!   `start-first`): a process that cannot serve yet does not pass it, and one that has
//!   served is not restarted for an outage of its state store or job service.
//! - `/readyz`: the server is ready to serve. 200 only when every check passes; else
//!   503 with the failing checks and why. The checks:
//!   - `stopping`: no stop signal has been received ([`Readiness::stop`]);
//!   - `leader`: this server holds its roles ([`Readiness::set_leader`]);
//!   - `store`: the object store answers a read of one fixed key within the probe
//!     timeout. The probe only reads (`get_range` of one byte), so a poll never writes
//!     to the bucket; an absent key is an answer, and so passes.
//!
//! - `/readyz/reapi`: what the REAPI cluster of a load balancer asks. `/readyz`'s checks, and
//!   one more, `no_executor`, on a server that runs no executor: it answers an Execute only
//!   from the action cache, so the check keeps REAPI calls off it. Whether the job service
//!   answers is a probe of `/readyz` itself on a frontend that has one.
//!
//! Once the server has given its roles away ([`Readiness::set_handed_over`], which a retire
//! does: `POST /v1/frontend/retire`, `docs/api.md`) the probes are not asked and the check
//! passes on `stopping` and `leader` alone: a probe could fail or wait out its timeout while
//! the server is still the one to answer the calls it kept open.
//!
//! The routes are served by the same future as the REAPI and worker listeners, which
//! are bound before it runs and stop when it returns, so an answer from either route
//! means both listeners are bound.
//!
//! The REAPI listener answers the same readiness over `grpc.health.v1` ([`Checks`] is
//! the one evaluation both use; see [`crate::grpc_health`]).
//!
//! The checks beyond the flags are [`Probe`]s, each with a name and a bounded wait: the
//! server's is its object store, a frontend's ([`crate::frontend`]) are its storage
//! node, its state store and the storage map. Where the worker listener checks callers
//! against the state store's deny rows, either has one more, `deny_rows`: the listener's copy
//! of them is one it may use (`crate::deny_rows`).
//!
//! A frontend answers its load balancer from the last answers of its probes
//! ([`Checks::begin`]): each probe is asked again on its own interval
//! (`--readyz-probe-interval-ms`), within its own timeout (`--readyz-store-timeout-ms`), and
//! `/readyz` reads what the last ask found, so no request waits for a probe. A probe that has
//! not answered within its timeout counts as failed. The stop signal, the leader role and
//! the hand-over are flags read at each request, not probes, so a drain turns `/readyz` to 503
//! at once. A server that has not called [`Checks::begin`] (`kbf-server` itself) asks its
//! probes on every request.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::future::{BoxFuture, join_all};
use kbf_front::{Cache, MetaLog, Routed};
use kbf_objstore::{ByteRange, ObjectStore, ObjectStoreError};
use kbf_storage::Storage;
use kbf_types::{Digest, DigestFunction};
use serde::Serialize;

use crate::api::JSON;
use crate::members::Members;
use crate::{BUILD_COMMIT, SERVER_VERSION};

/// The name, under the cache's key prefix, of the key the store probe reads. Nothing
/// writes it.
pub const PROBE_KEY: &str = "readyz-probe";

/// How long the store probe waits for the store unless `--readyz-store-timeout-ms`
/// says otherwise.
pub const STORE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a frontend asks each of its probes again unless `--readyz-probe-interval-ms`
/// says otherwise.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// What `/readyz` reads besides the store: whether a stop signal has come, and
/// whether this server holds its roles. Shared by the serving future, which
/// sets it, and the route and the gRPC health service, which read it. Every change is
/// announced to [`Readiness::subscribe`]rs, so a health `Watch` answers it at once.
pub struct Readiness {
    stopping: AtomicBool,
    leader: AtomicBool,
    handed_over: AtomicBool,
    changed: tokio::sync::watch::Sender<()>,
}

impl Default for Readiness {
    /// Not stopping, and the leader: a single server runs every role from its start.
    fn default() -> Self {
        Self {
            stopping: AtomicBool::new(false),
            leader: AtomicBool::new(true),
            handed_over: AtomicBool::new(false),
            changed: tokio::sync::watch::Sender::new(()),
        }
    }
}

impl Readiness {
    /// A stop signal has been received: `/readyz` answers 503 from now on. Returns
    /// whether this call was the first.
    pub fn stop(&self) -> bool {
        let first = !self.stopping.swap(true, Ordering::SeqCst);
        if first {
            self.changed.send_replace(());
        }
        first
    }

    /// This server is retiring and has stood its roles down: [`Checks::failing`] stops asking
    /// the probes. Readiness is otherwise untouched (it stays ready), and nothing undoes
    /// it. Returns whether this call was the first.
    pub fn set_handed_over(&self) -> bool {
        let first = !self.handed_over.swap(true, Ordering::SeqCst);
        if first {
            self.changed.send_replace(());
        }
        first
    }

    /// Whether [`Readiness::set_handed_over`] has been called.
    #[must_use]
    pub fn is_handed_over(&self) -> bool {
        self.handed_over.load(Ordering::SeqCst)
    }

    /// Whether a stop signal has been received.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Whether this server holds its roles. Nothing in the server clears it
    /// yet; it is the hook for a replicated control log, whose followers are not ready.
    pub fn set_leader(&self, leader: bool) {
        self.leader.store(leader, Ordering::SeqCst);
        self.changed.send_replace(());
    }

    /// Whether this server holds its roles.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.leader.load(Ordering::SeqCst)
    }

    /// A receiver that is marked changed whenever [`Readiness::stop`],
    /// [`Readiness::set_handed_over`] or [`Readiness::set_leader`] runs after this call.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }
}

/// One failing check of `/readyz`.
#[derive(Serialize)]
pub struct Failing {
    /// `stopping`, `leader`, `no_executor` (`/readyz/reapi` only), or the name of a probe (`store`; a frontend's `storage`,
    /// `state_store` and `map`; `deny_rows` where the worker listener checks the deny rows).
    pub check: &'static str,
    /// Why it fails.
    pub reason: String,
}

/// The body of `/readyz`.
#[derive(Serialize)]
struct ReadyBody {
    ready: bool,
    version: &'static str,
    commit: &'static str,
    failing: Vec<Failing>,
}

/// The body of `/startedz`.
#[derive(Serialize)]
struct StartedBody {
    started: bool,
    version: &'static str,
    commit: &'static str,
    failing: Vec<Failing>,
}

/// The body of `/healthz`.
#[derive(Serialize)]
struct AliveBody {
    status: &'static str,
    version: &'static str,
    commit: &'static str,
}

/// One dependency `/readyz` asks about, with the wait it is allowed.
pub trait Probe: Send + Sync {
    /// `Ok` when the dependency answers; else why not (a reason a person can act on).
    fn probe(&self) -> BoxFuture<'_, Result<(), String>>;
}

/// One [`Probe`] of [`Checks`], with the answer its last ask found.
struct Entry {
    check: &'static str,
    probe: Arc<dyn Probe>,
    /// The last answer. Until the first ask of [`Checks::begin`] has finished it is a
    /// failure, which nothing reads before then.
    last: tokio::sync::watch::Sender<Result<(), String>>,
}

impl Entry {
    /// Asks the probe once, waiting at most `timeout`, and keeps the answer: no answer in time
    /// is a failure.
    async fn refresh(&self, timeout: Duration) {
        let answer = match tokio::time::timeout(timeout, self.probe.probe()).await {
            Ok(answer) => answer,
            Err(_) => Err(format!(
                "the {} probe had no answer within {} ms",
                self.check,
                timeout.as_millis()
            )),
        };
        drop(self.last.send_replace(answer));
    }
}

/// Asks the probe of `entry` again every `interval`, for as long as the future is polled.
async fn keep(entry: Arc<Entry>, interval: Duration, timeout: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        entry.refresh(timeout).await;
    }
}

/// The checks of `/readyz`: the stop signal, the leader role and the [`Probe`]s, in
/// that order. One evaluation for the HTTP route and the `grpc.health.v1` service.
pub struct Checks {
    readiness: Arc<Readiness>,
    probes: Vec<Arc<Entry>>,
    /// Whether [`Checks::begin`] has run: the probes are then read from their last answers,
    /// not asked.
    background: AtomicBool,
    /// Whether [`Checks::failing`] has once found nothing failing (`/startedz`).
    started: AtomicBool,
}

impl Checks {
    /// The checks of `readiness` alone; add dependencies with [`Checks::probing`].
    #[must_use]
    pub const fn new(readiness: Arc<Readiness>) -> Self {
        Self {
            readiness,
            probes: Vec::new(),
            background: AtomicBool::new(false),
            started: AtomicBool::new(false),
        }
    }

    /// These checks, and `probe` under the name `check`.
    #[must_use]
    pub fn probing(mut self, check: &'static str, probe: Arc<dyn Probe>) -> Self {
        self.probes.push(Arc::new(Entry {
            check,
            probe,
            last: tokio::sync::watch::Sender::new(Err(
                "the probe has not been asked yet".to_owned()
            )),
        }));
        self
    }

    /// The checks of a server over `cache`: its store answers within `timeout`.
    #[must_use]
    pub fn of_cache<M, O>(
        cache: Arc<Cache<M, O>>,
        readiness: Arc<Readiness>,
        timeout: Duration,
    ) -> Self
    where
        M: MetaLog,
        O: ObjectStore + 'static,
    {
        Self::new(readiness).probing("store", Arc::new(StoreProbe { cache, timeout }))
    }

    /// The flags `/readyz` reads.
    #[must_use]
    pub fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    /// Asks every probe once, each within `timeout` and all at once, and from then on has
    /// [`Checks::failing`] answer from the last answers instead of asking. Returns the
    /// future that asks each probe again every `interval` (each on its own clock, so a probe
    /// that hangs delays no other), which never ends: spawn it for as long as the checks are
    /// served, and drop it to stop.
    pub async fn begin(&self, interval: Duration, timeout: Duration) -> BoxFuture<'static, ()> {
        join_all(self.probes.iter().map(|entry| entry.refresh(timeout))).await;
        self.background.store(true, Ordering::SeqCst);
        let keeping: Vec<_> = self
            .probes
            .iter()
            .map(|entry| keep(Arc::clone(entry), interval, timeout))
            .collect();
        Box::pin(async move {
            join_all(keeping).await;
        })
    }

    /// The checks that fail now, in the order the route lists them; empty when the
    /// server is ready. Asks each probe once, or, after [`Checks::begin`], reads the last
    /// answer of each and waits for none.
    pub async fn failing(&self) -> Vec<Failing> {
        let failing = self.failing_except(&[]).await;
        if failing.is_empty() {
            self.started.store(true, Ordering::SeqCst);
        }
        failing
    }

    /// Whether [`Checks::failing`] has once found nothing failing: the server has been ready.
    #[must_use]
    pub fn has_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// [`Checks::failing`] without asking the probes named in `skip`: what a caller that
    /// does not depend on them (a frontend's `READY=1`, which does not wait on storage)
    /// waits for.
    pub async fn failing_except(&self, skip: &[&str]) -> Vec<Failing> {
        let mut failing = Vec::new();
        if self.readiness.is_stopping() {
            failing.push(Failing {
                check: "stopping",
                reason: "a stop signal was received; the server is shutting down".to_owned(),
            });
        }
        if !self.readiness.is_leader() {
            failing.push(Failing {
                check: "leader",
                reason: "this server does not hold the scheduler role".to_owned(),
            });
        }
        if self.readiness.is_handed_over() {
            return failing;
        }
        let background = self.background.load(Ordering::SeqCst);
        for entry in &self.probes {
            if skip.contains(&entry.check) {
                continue;
            }
            let last = background.then(|| entry.last.borrow().clone());
            let answer = match last {
                Some(answer) => answer,
                None => entry.probe.probe().await,
            };
            if let Err(reason) = answer {
                failing.push(Failing {
                    check: entry.check,
                    reason,
                });
            }
        }
        failing
    }
}

/// The object store of a cache, read as [`probe_store`] does.
struct StoreProbe<M, O> {
    cache: Arc<Cache<M, O>>,
    timeout: Duration,
}

impl<M: MetaLog, O: ObjectStore + 'static> Probe for StoreProbe<M, O> {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(probe_store(&self.cache, self.timeout))
    }
}

/// The action digest the storage probe asks a node about: the SHA-256 field all zero,
/// which no action has, so the node answers "no entry" from its index and the call
/// writes nothing.
fn probe_digest() -> Digest {
    Digest::new(DigestFunction::Sha256, [0; 32], 1)
}

/// A storage node, asked for the action-cache entry of [`probe_digest`]: a round trip
/// through the transport, the node's certificate check, its check of the role in the
/// frontend's certificate (reading the action cache is the one call only a frontend may
/// make) and its index.
///
/// Once the frontend routes by the storage map ([`StorageProbe::routed`] and
/// [`Routed::active`]) the question goes to the owners the map names for
/// [`probe_digest`], in order, and the check passes when one of them answers: the
/// `--storage` node may have left the farm, and a farm whose other members are up is
/// ready. Before that it is the `--storage` node alone.
pub struct StorageProbe {
    storage: Arc<dyn Storage>,
    routed: Option<Arc<Routed>>,
    timeout: Duration,
}

impl StorageProbe {
    /// A probe of `storage`, waiting at most `timeout` for its answer.
    #[must_use]
    pub fn new(storage: Arc<dyn Storage>, timeout: Duration) -> Self {
        Self {
            storage,
            routed: None,
            timeout,
        }
    }

    /// The same probe, asking the owners `routed` names instead of `storage` while it is
    /// routing by the map.
    #[must_use]
    pub fn routed(mut self, routed: Arc<Routed>) -> Self {
        self.routed = Some(routed);
        self
    }

    /// The action-cache lookup, at the owners or at the one node.
    async fn ask(&self) -> Result<(), kbf_storage::StorageError> {
        let digest = probe_digest();
        if let Some(routed) = &self.routed {
            routed.tag_bootstrap();
        }
        match self.routed.as_ref().filter(|routed| routed.active()) {
            Some(routed) => routed.router().action_result(&digest).await.map(drop),
            None => self.storage.action_result(&digest).await.map(drop),
        }
    }
}

impl Probe for StorageProbe {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            match tokio::time::timeout(self.timeout, self.ask()).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(format!("the storage node did not answer: {e}")),
                Err(_) => Err(format!(
                    "the storage node had no answer within {} ms",
                    self.timeout.as_millis()
                )),
            }
        })
    }
}

/// The state store, read at [`PROBE_KEY`] (an absent key is an answer).
pub struct StateProbe {
    store: Arc<dyn kbf_state::Store>,
    timeout: Duration,
}

impl StateProbe {
    /// A probe of `store`, waiting at most `timeout` for its answer.
    #[must_use]
    pub fn new(store: Arc<dyn kbf_state::Store>, timeout: Duration) -> Self {
        Self { store, timeout }
    }
}

impl Probe for StateProbe {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let store = Arc::clone(&self.store);
            let read = tokio::task::spawn_blocking(move || store.get(PROBE_KEY));
            match tokio::time::timeout(self.timeout, read).await {
                Ok(Ok(Ok(_))) => Ok(()),
                Ok(Ok(Err(e))) => Err(format!("a read of {PROBE_KEY} failed: {e}")),
                Ok(Err(e)) => Err(format!("a read of {PROBE_KEY} did not finish: {e}")),
                Err(_) => Err(format!(
                    "a read of {PROBE_KEY} had no answer within {} ms",
                    self.timeout.as_millis()
                )),
            }
        })
    }
}

/// Whether the storage map is loaded: true today, because a frontend has one storage
/// node (`--storage`) and no map to load. TODO(av2-08): the map arrives from the state
/// store (`docs/design/architecture-v2.md`, section 6); this probe then fails until it
/// has been read.
pub struct MapProbe;

impl Probe for MapProbe {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// The `/healthz` and `/readyz` routes; `/readyz` probes `cache`'s store, waiting at
/// most `timeout` for it.
pub fn router<M, O>(cache: Arc<Cache<M, O>>, readiness: Arc<Readiness>, timeout: Duration) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    router_of(cache_checks(cache, readiness, timeout))
}

/// The checks of a server over `cache` ([`Checks::of_cache`]), shared by the routes that
/// report them.
pub fn cache_checks<M, O>(
    cache: Arc<Cache<M, O>>,
    readiness: Arc<Readiness>,
    timeout: Duration,
) -> Arc<Checks>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Arc::new(Checks::of_cache(cache, readiness, timeout))
}

/// The health routes over `checks`: `/healthz`, `/readyz`, `/readyz/reapi` (which also fails
/// while `executes` is false: this server runs no executor) and `GET /v1/health`, which
/// composes `checks` and the storage health of `members` (none: this server keeps no storage
/// members) ([`crate::health_summary`]).
pub fn routes(checks: &Arc<Checks>, executes: bool, members: Option<&Arc<Members>>) -> Router {
    let summary = crate::health_summary::route(Arc::clone(checks), members.cloned());
    let reapi = Router::new()
        .route(READYZ_REAPI, get(readyz_reapi))
        .with_state(Arc::new(ReapiReady {
            checks: Arc::clone(checks),
            executes,
        }));
    router_of(Arc::clone(checks)).merge(summary).merge(reapi)
}

/// The path of `GET /readyz/reapi`.
pub const READYZ_REAPI: &str = "/readyz/reapi";

/// What `GET /readyz/reapi` reads.
struct ReapiReady {
    checks: Arc<Checks>,
    /// Whether this server runs an executor.
    executes: bool,
}

/// The check `/readyz/reapi` adds to `/readyz`'s.
const NO_EXECUTOR: &str = "no_executor";

/// The path of `GET /startedz`.
pub const STARTEDZ: &str = "/startedz";

/// The `/healthz`, `/startedz` and `/readyz` routes over `checks`.
pub fn router_of(checks: Arc<Checks>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(STARTEDZ, get(startedz))
        .route("/readyz", get(readyz))
        .with_state(checks)
}

async fn healthz() -> Response {
    let body = AliveBody {
        status: "alive",
        version: SERVER_VERSION,
        commit: BUILD_COMMIT,
    };
    json(StatusCode::OK, &body)
}

/// 200 once the server has been ready; until then `/readyz`'s answer, asked now.
async fn startedz(State(checks): State<Arc<Checks>>) -> Response {
    let failing = if checks.has_started() {
        Vec::new()
    } else {
        checks.failing().await
    };
    let started = failing.is_empty();
    let code = if started {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = StartedBody {
        started,
        version: SERVER_VERSION,
        commit: BUILD_COMMIT,
        failing,
    };
    json(code, &body)
}

async fn readyz(State(checks): State<Arc<Checks>>) -> Response {
    ready_answer(checks.failing().await)
}

/// `/readyz`'s answer, and `no_executor` on a server that runs no executor: it answers no
/// Execute but from the action cache. Whether the job service answers is a check of
/// `/readyz` itself, where the server has one.
async fn readyz_reapi(State(state): State<Arc<ReapiReady>>) -> Response {
    let mut failing = state.checks.failing().await;
    if !state.executes {
        failing.push(Failing {
            check: NO_EXECUTOR,
            reason: "this server runs no executor (--executor js-agent)".to_owned(),
        });
    }
    ready_answer(failing)
}

fn ready_answer(failing: Vec<Failing>) -> Response {
    let ready = failing.is_empty();
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = ReadyBody {
        ready,
        version: SERVER_VERSION,
        commit: BUILD_COMMIT,
        failing,
    };
    json(code, &body)
}

/// Reads the first byte of [`PROBE_KEY`] under `cache`'s prefix, waiting at most
/// `timeout`. The store is reachable if it answers with the byte, that the key is
/// absent, or that the range is past its end; any other answer, or none in time, is
/// the reason it is not.
///
/// # Errors
/// The store's error, or that it did not answer within `timeout`.
pub async fn probe_store<M, O>(cache: &Cache<M, O>, timeout: Duration) -> Result<(), String>
where
    M: MetaLog,
    O: ObjectStore,
{
    let prefix = cache.prefix();
    let probe = prefix.key(PROBE_KEY).ok().zip(ByteRange::new(0, 1));
    let Some((key, first)) = probe else {
        return Err(format!(
            "{PROBE_KEY} under the prefix ({} bytes) is not a valid key",
            prefix.as_str().len()
        ));
    };
    match tokio::time::timeout(timeout, cache.objects().get_range(&key, first)).await {
        Ok(Ok(_) | Err(ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. })) => {
            Ok(())
        }
        Ok(Err(e)) => Err(format!("a read of {key} failed: {e}")),
        Err(_) => Err(format!(
            "a read of {key} had no answer within {} ms",
            timeout.as_millis()
        )),
    }
}

fn json(code: StatusCode, value: &impl Serialize) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    (code, [(CONTENT_TYPE, JSON)], body).into_response()
}

#[cfg(test)]
mod tests;
