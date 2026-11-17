//! The integration-test harness: an in-process single-node `kbf-server` on loopback ports whose
//! Execute runs on the real executor over a fake job service, and REAPI clients for it.
//!
//! # What a [`Cell`] is
//!
//! `Cell::start()` binds a server (`bind_server_with_state`) on its own runtime whose
//! `Listeners::executor` is a [`kbf_server::executor::Executor`] over
//! [`kbf_job_client::fake::FakeJobServer`] (one Linux x86-64 host, [`HOST`]), and fakes of the
//! seams the executor reads the rest of the server through: the inputs (an image, the action's
//! timeout, its tree) and the ingest. The ingest stands for the task
//! tool's pack: it answers with the [`Ran`] the test scripted for the action (nothing scripted:
//! exit 0, no outputs), stores its blobs in the cell's CAS and, for an exit of 0, writes the
//! result to the cell's action cache, as the frontend's ingest does. `cell.direct` holds the job
//! service, the scripts and the drive helpers; it survives [`Cell::restart`] and
//! [`Cell::cold_restart`], each of which builds a new executor over the same job service.
//!
//! Other starts: [`Cell::start_without_executor`] (an Execute the cache does not answer is
//! refused FAILED_PRECONDITION), [`Cell::start_with_state`] over a [`FakeState`] (the executor
//! keeps its operations' rows in it; its clock is the store's, moved with [`FakeState::advance`])
//! or any `kbf_state::Store` in a [`StateHandle`], [`Cell::start_raft`], [`Cell::start_with_api`],
//! [`Cell::start_with_retention`], [`Cell::start_with_streams`].
//!
//! # Driving a job
//!
//! An Execute submits one job; `direct.job(&action, attempt)` waits for it and names it,
//! `direct.run(&action)` places it on the host and runs it, `direct.finish(&action, n)` ends it
//! with the task tool's exit `n` (0: the pack is ingested; anything else is an infrastructure
//! ending the executor runs again, as the next attempt), `direct.complete(&action)` is a clean
//! end, `direct.lose(&action, attempt)` loses the host, `direct.time_out(&action)` ends the job
//! at its wall limit. `direct.jobs_of(&action)` lists the attempts; `direct.fake` is the job
//! service itself for anything finer (faults, its clock, its calls).
//!
//! Time here is real, bounded by [`PROMPT`] where a test waits for an event; the executor polls
//! the job service every 20 ms. A test never asserts how long anything took.
//!
//! # Example
//!
//! ```ignore
//! mod support;
//! use support::{Cell, Job, Ran, done, response};
//!
//! #[tokio::test]
//! async fn an_action_runs_as_one_job() {
//!     let cell = Cell::start().await;
//!     let job = Job::new("/bin/run", &[]);
//!     cell.upload(&job.blobs()).await;
//!     cell.direct.will_run(&job.action, Ran::writes("out", "hello").stdout("log"));
//!
//!     let mut ops = cell.execute(&job.action).await;
//!     cell.direct.complete(&job.action).await;
//!     let result = response(&done(&mut ops).await).result.expect("a result");
//!
//!     assert_eq!(result.exit_code, 0);
//!     assert_eq!(cell.cached(&job.action).await.expect("cached"), result);
//! }
//! ```

#![allow(dead_code)]

use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use kbf_front::{Cache, Leadership, MAX_MESSAGE_BYTES, MemoryMetaLog, MetaLog, MetaLogError};
use kbf_meta::{Applied, Command as MetaCommand, MetaState, Retention};
use kbf_objstore::KeyPrefix;
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    self, Action, ActionResult, BatchUpdateBlobsRequest, Command, Directory, ExecuteRequest,
    ExecuteResponse, FileNode, FindMissingBlobsRequest, GetActionResultRequest, OutputFile,
    Platform, batch_update_blobs_request, platform,
};
use kbf_server::token::ApiToken;
use kbf_server::{
    Api, Bound, Listeners, RaftMetaLog, ServeError, StreamLimits, bind_server_with_state,
};
use kbf_store::FaultFs;
use prost::Message;
use tokio::sync::{Notify, oneshot};
use tokio::time::timeout;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Streaming};

mod direct;
mod store;
// Each test binary takes what it uses of the harness.
#[allow(unused_imports)]
pub use direct::{Direct, HOST, Ingestion, Ran, machine};
pub use store::SharedStore;

/// How long a test waits for something that should happen promptly.
pub const PROMPT: Duration = Duration::from_secs(5);

/// How long a test watches for something that must not happen. Several ticks and
/// heartbeat intervals.
pub const QUIET: Duration = Duration::from_millis(400);

/// The heartbeat interval the test server names in `Welcome`.
pub const INTERVAL: Duration = Duration::from_millis(100);

/// A point a test can close: the next caller to reach it waits there until the test
/// opens it, so the test can act while the server is in the middle of that step.
#[derive(Debug, Default)]
pub struct Gate {
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl Gate {
    /// Holds the next caller at the gate.
    pub fn close_next(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Waits until a caller is held at the gate.
    pub async fn held(&self) {
        timeout(PROMPT, self.entered.notified())
            .await
            .expect("a caller reaches the gate");
    }

    /// Lets the held caller through.
    pub fn open(&self) {
        self.release.notify_one();
    }

    async fn pass(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

/// The metadata log, in memory or a Raft log, with gates a test can close on the next
/// query and on the next action-cache write, and a switch that makes the next
/// action-cache write fail as an unreachable leader would.
#[derive(Debug)]
pub struct GateLog {
    inner: Inner,
    pub query: Gate,
    pub action_write: Gate,
    fail_action_write: AtomicBool,
}

#[derive(Debug)]
enum Inner {
    Memory(Box<MemoryMetaLog>),
    Raft(RaftMetaLog<FaultFs>),
}

impl GateLog {
    fn new(inner: Inner) -> Self {
        Self {
            inner,
            query: Gate::default(),
            action_write: Gate::default(),
            fail_action_write: AtomicBool::new(false),
        }
    }

    /// The Raft log, for a cell started with one ([`Cell::start_raft`]).
    pub fn raft(&self) -> Option<&RaftMetaLog<FaultFs>> {
        match &self.inner {
            Inner::Memory(_) => None,
            Inner::Raft(log) => Some(log),
        }
    }

    /// Fails the next action-cache write with `MetaLogError::Unavailable`.
    pub fn fail_next_action_write(&self) {
        self.fail_action_write.store(true, Ordering::SeqCst);
    }
}

impl MetaLog for GateLog {
    async fn commit(&self, command: MetaCommand) -> Result<Applied, MetaLogError> {
        if matches!(command, MetaCommand::PutAction { .. }) {
            self.action_write.pass().await;
            if self.fail_action_write.swap(false, Ordering::SeqCst) {
                return Err(MetaLogError::Unavailable("failed by the test".to_owned()));
            }
        }
        match &self.inner {
            Inner::Memory(log) => log.commit(command).await,
            Inner::Raft(log) => log.commit(command).await,
        }
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.query.pass().await;
        match &self.inner {
            Inner::Memory(log) => log.query(f).await,
            Inner::Raft(log) => log.query(f).await,
        }
    }

    fn leadership(&self) -> Option<Leadership> {
        match &self.inner {
            Inner::Memory(log) => log.leadership(),
            Inner::Raft(log) => log.leadership(),
        }
    }
}

/// What a server is started with: the knobs the `Cell::start_*` constructors turn.
#[derive(Clone)]
struct Setup {
    api: Option<Api>,
    raft_dir: Option<FaultFs>,
    retention: Duration,
    streams: StreamLimits,
    /// The state store, which the executor keeps its operations' rows in too.
    state: Option<Arc<StateHandle>>,
    /// Whether the server has an executor (`Listeners::executor`).
    executor: bool,
}

/// A state store a cell is started over, which every restart of the cell opens again.
pub struct StateHandle {
    pub store: Arc<dyn kbf_state::Store>,
}

impl StateHandle {
    fn open(&self) -> kbf_server::StateStore {
        kbf_server::StateStore {
            store: Arc::clone(&self.store),
            members: Default::default(),
            history: None,
        }
    }
}

/// An in-memory state store, and the clock its rows expire on, which a test moves by hand.
pub struct FakeState {
    pub clock: Arc<kbf_state::ManualClock>,
    pub store: Arc<kbf_state::fake::Fake>,
}

impl FakeState {
    pub fn new() -> Self {
        let clock = Arc::new(kbf_state::ManualClock::default());
        let store = Arc::new(kbf_state::fake::Fake::with_flaw(
            Arc::clone(&clock),
            kbf_state::fake::Flaw::None,
        ));
        Self { clock, store }
    }

    /// The store as a cell takes it.
    pub fn handle(&self) -> Arc<StateHandle> {
        Arc::new(StateHandle {
            store: Arc::clone(&self.store) as Arc<dyn kbf_state::Store>,
        })
    }

    /// Time passes on the store's clock.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// Every key under `prefix`.
    pub fn keys(&self, prefix: &str) -> Vec<String> {
        use kbf_state::Store as _;
        let rows = self.store.scan_prefix(prefix).expect("a scan");
        rows.into_iter().map(|(key, _)| key).collect()
    }
}

/// A running server in this process, and a [`Client`] of it.
pub struct Cell {
    pub cache: Arc<Cache<GateLog, SharedStore>>,
    /// The bucket, shared with every restart of this cell.
    pub store: SharedStore,
    /// The job service the server's executor submits to, the scripts of its actions, and the
    /// helpers that drive a job. Shared with every restart of this cell.
    pub direct: Direct,
    /// The operator API's address, for a cell started with one.
    pub api: Option<SocketAddr>,
    /// What the cell was started with, kept for a restart.
    setup: Setup,
    /// The raft directory of a cell started with [`Cell::start_raft`], as the old
    /// process left it.
    raft_dir: Option<FaultFs>,
    /// Stops the server; taken by [`Cell::cold_restart`], sent on drop otherwise.
    stop: Option<oneshot::Sender<()>>,
    /// Answers once the server's runtime, and every task on it, is gone.
    stopped: Option<oneshot::Receiver<()>>,
    /// The server's invocation records.
    pub recorder: kbf_front::Recorder,
    /// The server's frontend drain.
    pub drain: Arc<kbf_server::Drain>,
    client: Client,
}

impl Drop for Cell {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Deref for Cell {
    type Target = Client;

    fn deref(&self) -> &Client {
        &self.client
    }
}

/// Channels to a server's two listeners, and REAPI helpers.
pub struct Client {
    pub reapi_addr: SocketAddr,
    pub worker_addr: SocketAddr,
    channel: Channel,
}

/// The bearer token of a cell's operator API ([`Cell::start_with_api`]).
pub const API_TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

impl Setup {
    fn new() -> Self {
        Self {
            api: None,
            raft_dir: None,
            retention: kbf_server::executor::FINISHED_RETENTION,
            streams: StreamLimits::DEFAULT,
            state: None,
            executor: true,
        }
    }
}

impl Cell {
    /// A server with an executor over a fresh fake job service.
    pub async fn start() -> Self {
        Self::fresh(Setup::new()).await
    }

    /// A server with no executor: an Execute the action cache does not answer is refused
    /// FAILED_PRECONDITION.
    pub async fn start_without_executor() -> Self {
        Self::fresh(Setup {
            executor: false,
            ..Setup::new()
        })
        .await
    }

    /// A cell whose listeners allow `streams` HTTP/2 streams per connection
    /// (`--reapi-max-streams`, `--worker-max-streams`).
    pub async fn start_with_streams(streams: StreamLimits) -> Self {
        Self::fresh(Setup {
            streams,
            ..Setup::new()
        })
        .await
    }

    /// A cell whose executor keeps its operations' rows in `state` ([`FakeState::handle`], or a
    /// SQLite store of the test's own in a [`StateHandle`]), and that serves the operator API
    /// without a token: its `/metrics` is at [`Cell::api`].
    pub async fn start_with_state(state: Arc<StateHandle>) -> Self {
        Self::fresh(Setup {
            api: Some(Api {
                listen: SocketAddr::from(([127, 0, 0, 1], 0)),
                token: None,
            }),
            state: Some(state),
            ..Setup::new()
        })
        .await
    }

    /// [`Cell::start_with_state`], whose operator API takes writes with [`API_TOKEN`].
    pub async fn start_with_api_and_state(state: Arc<StateHandle>) -> Self {
        Self::fresh(Setup {
            api: Some(Api {
                listen: SocketAddr::from(([127, 0, 0, 1], 0)),
                token: Some(api_token()),
            }),
            state: Some(state),
            ..Setup::new()
        })
        .await
    }

    /// A cell that keeps a finished operation for `retention`.
    pub async fn start_with_retention(retention: Duration) -> Self {
        Self::fresh(Setup {
            retention,
            ..Setup::new()
        })
        .await
    }

    /// A cell whose metadata is a Raft log ([`RaftMetaLog`], as `kbf-server` runs it) in
    /// an in-memory raft directory, which [`Cell::cold_restart`] carries over as a
    /// process restart leaves it. Objects go under `kbf/<log id>/`.
    pub async fn start_raft() -> Self {
        let dir = FaultFs::new();
        Self::fresh(Setup {
            raft_dir: Some(dir),
            ..Setup::new()
        })
        .await
    }

    /// The Raft log of a cell started with [`Cell::start_raft`].
    pub fn raft_log(&self) -> &RaftMetaLog<FaultFs> {
        self.cache.meta().raft().expect("a cell with a Raft log")
    }

    /// A cell that also serves the operator API, whose writes need [`API_TOKEN`].
    pub async fn start_with_api() -> Self {
        Self::fresh(Setup {
            api: Some(Api {
                listen: SocketAddr::from(([127, 0, 0, 1], 0)),
                token: Some(api_token()),
            }),
            ..Setup::new()
        })
        .await
    }

    async fn fresh(setup: Setup) -> Self {
        let store = SharedStore::new();
        let cache = Self::cold_cache(&store, setup.raft_dir.as_ref()).await;
        let direct = Direct::start().await;
        Self::serve(cache, store, direct, setup).await
    }

    /// A new server over this cell's store, its in-memory action cache and CAS index, and its
    /// job service, with a new executor: no operation of the old one is known to it (with a
    /// state store, it takes up the old one's unfinished operations once the old one's
    /// liveness row expires). This one is left running. [`Cell::cold_restart`] is a restart as
    /// the binary has it. On a cell started with [`Cell::start_raft`] both servers share one
    /// open Raft log, and so its term: use [`Cell::cold_restart`] there.
    pub async fn restart(&self) -> Self {
        let (cache, store) = (Arc::clone(&self.cache), self.store.clone());
        let setup = Setup {
            raft_dir: self.raft_dir.clone(),
            ..self.setup.clone()
        };
        Self::serve(cache, store, self.direct.clone(), setup).await
    }

    /// Stops this server, waits until its runtime and every task on it are gone (each
    /// connection and stream of it ends, as when the process exits), and starts a new
    /// one over the same bucket and the same job service, as a restart of the `kbf-server`
    /// binary does: a new `Cache` with a new, empty metadata log (so an empty CAS index and
    /// action cache), a new executor, and its objects written under a key prefix no earlier
    /// start used, as an S3 cold store does. The objects earlier starts wrote stay in the
    /// bucket, and the jobs the job service holds go on. The operator API, if this cell has
    /// one, listens again on a new port.
    ///
    /// A cell started with [`Cell::start_raft`] reopens its Raft log from the raft
    /// directory as the old process left it (the Raft metadata log), so the CAS index and
    /// the action cache are kept, and writes under the log's prefix in a new epoch.
    pub async fn cold_restart(mut self) -> Self {
        let stop = self.stop.take().expect("the server is running");
        let stopped = self.stopped.take().expect("the server is running");
        let (store, direct) = (self.store.clone(), self.direct.clone());
        let dir = self.raft_dir.take();
        let setup = self.setup.clone();
        drop(self);
        stop.send(()).expect("the server is running");
        timeout(PROMPT * 4, stopped)
            .await
            .expect("the old server stops in time")
            .expect("the old server stops cleanly");
        let dir = dir.map(|d| d.restart());
        let cache = Self::cold_cache(&store, dir.as_ref()).await;
        let setup = Setup {
            raft_dir: dir,
            ..setup
        };
        Self::serve(cache, store, direct, setup).await
    }

    /// A new cache over `store` and a writer epoch it allocates. Without `raft_dir`:
    /// an empty metadata log, and objects under `start-<n>/`, where `n` is new to this
    /// test process. With it: the Raft log there (a new one, with id `n + 1`, if the
    /// directory is empty), and objects under the log's prefix.
    async fn cold_cache(
        store: &SharedStore,
        raft_dir: Option<&FaultFs>,
    ) -> Arc<Cache<GateLog, SharedStore>> {
        static STARTS: AtomicU32 = AtomicU32::new(0);
        let n = STARTS.fetch_add(1, Ordering::Relaxed);
        let (log, prefix) = match raft_dir {
            None => {
                let prefix = KeyPrefix::new(format!("start-{n}/")).expect("a key prefix");
                (
                    Inner::Memory(Box::new(MemoryMetaLog::new(Retention::default()))),
                    prefix,
                )
            }
            Some(dir) => {
                let id = || {
                    Ok(kbf_log::FarmLogId::new(
                        (u64::from(n) + 1).try_into().unwrap(),
                    ))
                };
                let log = RaftMetaLog::open(dir.clone(), Retention::default(), "kbf/", id).await;
                let log = log.expect("open the Raft log");
                let prefix = log.store_prefix();
                (Inner::Raft(log), prefix)
            }
        };
        let cache = Cache::open(GateLog::new(log), store.clone(), prefix).await;
        Arc::new(cache.expect("open the cache"))
    }

    async fn serve(
        cache: Arc<Cache<GateLog, SharedStore>>,
        store: SharedStore,
        direct: Direct,
        setup: Setup,
    ) -> Self {
        let (stop, stop_rx) = oneshot::channel::<()>();
        let (stopped_tx, stopped) = oneshot::channel::<()>();
        let (bound_tx, bound_rx) = oneshot::channel();
        let (server_cache, server_api) = (Arc::clone(&cache), setup.api.clone());
        let (server_direct, server_setup) = (direct.clone(), setup.clone());
        // The server runs on a runtime of its own, as a process apart: once it stops,
        // the runtime is dropped and every task of it (each connection, each stream, each
        // operation of the executor) goes with it, as when the process exits.
        std::thread::Builder::new()
            .name("kbf-server-cell".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime");
                runtime.block_on(async move {
                    let shutdown = async move {
                        let _ = stop_rx.await;
                    };
                    let state = server_setup.state.as_ref().map(|s| s.open());
                    let executor = server_setup.executor.then(|| {
                        let rows = state.as_ref().map(|s| Arc::clone(&s.store));
                        server_direct.executor(
                            Arc::clone(&server_cache),
                            rows,
                            server_setup.retention,
                        )
                    });
                    let listeners = Listeners {
                        reapi: SocketAddr::from(([127, 0, 0, 1], 0)),
                        worker: SocketAddr::from(([127, 0, 0, 1], 0)),
                        reapi_tls: None,
                        worker_tls: None,
                        executor,
                        finished_retention: server_setup.retention,
                        shutdown_timeout: Duration::from_secs(10),
                        streams: kbf_server::StreamTiming::default(),
                        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
                        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
                        stream_limits: server_setup.streams,
                    };
                    let bound = bind_server_with_state(
                        server_cache,
                        listeners,
                        server_api,
                        kbf_auth::Policy::allow_all(),
                        state,
                        shutdown,
                    );
                    run(bound.expect("bind"), bound_tx).await;
                });
                drop(runtime);
                let _ = stopped_tx.send(());
            })
            .expect("a server thread");
        let (reapi, worker, api, recorder, drain) = bound_rx.await.expect("the server binds");
        Self {
            cache,
            store,
            direct,
            api,
            raft_dir: setup.raft_dir.clone(),
            setup,
            stop: Some(stop),
            stopped: Some(stopped),
            recorder,
            drain,
            client: Client::connect(reapi, worker).await,
        }
    }
}

/// What a started server tells the test: its REAPI, worker and operator API addresses,
/// its invocation records and its frontend drain.
type Started = (
    SocketAddr,
    SocketAddr,
    Option<SocketAddr>,
    kbf_front::Recorder,
    Arc<kbf_server::Drain>,
);

/// Says where `bound` listens on `addresses`, then serves it until it stops.
async fn run(
    bound: Bound<impl Future<Output = Result<(), ServeError>>>,
    addresses: oneshot::Sender<Started>,
) {
    let recorder = bound.recorder.clone();
    let _ = addresses.send((
        bound.reapi,
        bound.worker,
        bound.api,
        recorder,
        Arc::clone(&bound.drain),
    ));
    bound.serving.await.expect("serve");
}

/// An operator API token holding [`API_TOKEN`], from a file of mode 0600 unique to
/// this call.
fn api_token() -> ApiToken {
    use std::os::unix::fs::PermissionsExt;
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-cell");
    let path = dir.join("token");
    std::fs::write(&path, format!("{API_TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

impl Client {
    /// A client of the server listening at `reapi` and `worker`.
    pub async fn connect(reapi: SocketAddr, worker: SocketAddr) -> Self {
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        Self {
            reapi_addr: reapi,
            worker_addr: worker,
            channel,
        }
    }

    /// The channel to the REAPI listener.
    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }

    pub fn exec(&self) -> ExecutionClient<Channel> {
        ExecutionClient::new(self.channel.clone()).max_decoding_message_size(MAX_MESSAGE_BYTES)
    }

    pub fn ac(&self) -> ActionCacheClient<Channel> {
        ActionCacheClient::new(self.channel.clone())
    }

    pub async fn upload(&self, blobs: &[&Blob]) {
        let response = ContentAddressableStorageClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
            .batch_update_blobs(BatchUpdateBlobsRequest {
                requests: blobs
                    .iter()
                    .map(|b| batch_update_blobs_request::Request {
                        digest: Some(b.proto.clone()),
                        data: b.data.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .await
            .expect("BatchUpdateBlobs")
            .into_inner();
        for r in response.responses {
            assert_eq!(
                r.status.expect("status").code,
                0,
                "upload of {:?}",
                r.digest
            );
        }
    }

    /// Whether the CAS holds `blob`.
    pub async fn holds(&self, blob: &Blob) -> bool {
        ContentAddressableStorageClient::new(self.channel.clone())
            .find_missing_blobs(FindMissingBlobsRequest {
                blob_digests: vec![blob.proto.clone()],
                ..Default::default()
            })
            .await
            .expect("FindMissingBlobs")
            .into_inner()
            .missing_blob_digests
            .is_empty()
    }

    /// Starts an Execute of `action`.
    pub async fn execute(&self, action: &Blob) -> Streaming<Operation> {
        self.execute_with(action, false).await.expect("Execute")
    }

    pub async fn execute_with(
        &self,
        action: &Blob,
        skip_cache_lookup: bool,
    ) -> Result<Streaming<Operation>, tonic::Status> {
        self.exec()
            .execute(ExecuteRequest {
                instance_name: "main".to_owned(),
                action_digest: Some(action.proto.clone()),
                skip_cache_lookup,
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
    }

    /// GetActionResult: the result as the fake ingest gave it ([`unstamped`]), or the
    /// error code.
    pub async fn cached(&self, action: &Blob) -> Result<ActionResult, Code> {
        self.cached_stamped(action).await.map(unstamped)
    }

    /// GetActionResult: the result as the cache holds it, with the server's metadata.
    pub async fn cached_stamped(&self, action: &Blob) -> Result<ActionResult, Code> {
        self.ac()
            .get_action_result(GetActionResultRequest {
                action_digest: Some(action.proto.clone()),
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| s.code())
    }
}

/// A blob: its bytes and its digest in both forms.
#[derive(Clone, Debug)]
pub struct Blob {
    pub data: Vec<u8>,
    pub digest: kbf_types::Digest,
    pub proto: reapi::Digest,
}

impl Blob {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        let data = data.into();
        let digest = kbf_segments::sha256(&data);
        let proto = kbf_front::digest_to_proto(&digest);
        Self {
            data,
            digest,
            proto,
        }
    }

    pub fn of(message: &impl Message) -> Self {
        Self::new(message.encode_to_vec())
    }
}

/// An action and every blob it needs.
pub struct Job {
    pub action: Blob,
    pub command: Blob,
    pub root: Blob,
    pub input: Blob,
}

impl Job {
    /// A job running `argv`, with one input file, on `platform` properties.
    pub fn new(argv: &str, platform: &[(&str, &str)]) -> Self {
        let input = Blob::new(format!("input of {argv}"));
        let root = Blob::of(&Directory {
            files: vec![FileNode {
                name: "in".to_owned(),
                digest: Some(input.proto.clone()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let command = Blob::of(&Command {
            arguments: vec![argv.to_owned()],
            ..Default::default()
        });
        let action = Blob::of(&Action {
            command_digest: Some(command.proto.clone()),
            input_root_digest: Some(root.proto.clone()),
            platform: Some(Platform {
                properties: platform
                    .iter()
                    .map(|(name, value)| platform::Property {
                        name: (*name).to_owned(),
                        value: (*value).to_owned(),
                    })
                    .collect(),
            }),
            ..Default::default()
        });
        Self {
            action,
            command,
            root,
            input,
        }
    }

    pub fn blobs(&self) -> [&Blob; 4] {
        [&self.action, &self.command, &self.root, &self.input]
    }
}

/// Uploads an output and returns an ActionResult naming it, with `exit_code`.
pub async fn output(cell: &Client, text: &str, exit_code: i32) -> ActionResult {
    let out = Blob::new(text);
    cell.upload(&[&out]).await;
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(out.proto.clone()),
            ..Default::default()
        }],
        exit_code,
        ..Default::default()
    }
}

/// The ExecuteResponse of a done operation, its result as the fake ingest gave it
/// ([`unstamped`]).
pub fn response(op: &Operation) -> ExecuteResponse {
    let mut response = stamped_response(op);
    response.result = response.result.map(unstamped);
    response
}

/// `result` without its execution metadata, whatever put it there (the task tool's own, or
/// the front's): what is left is what the fake ingest ([`Ran`]) gave back.
pub fn unstamped(mut result: ActionResult) -> ActionResult {
    result.execution_metadata = None;
    result
}

/// The ExecuteResponse of a done operation, as the server sent it.
pub fn stamped_response(op: &Operation) -> ExecuteResponse {
    assert!(op.done, "{op:?} is not done");
    match &op.result {
        Some(operation::Result::Response(any)) => {
            ExecuteResponse::decode(any.value.as_slice()).expect("an ExecuteResponse")
        }
        other => panic!("a done operation without a response: {other:?}"),
    }
}

/// The stage of an operation, from its metadata.
pub fn stage(op: &Operation) -> i32 {
    let any = op.metadata.as_ref().expect("metadata");
    reapi::ExecuteOperationMetadata::decode(any.value.as_slice())
        .expect("ExecuteOperationMetadata")
        .stage
}

/// Reads `ops` until the done operation, which it returns.
pub async fn done(ops: &mut Streaming<Operation>) -> Operation {
    loop {
        let op = timeout(PROMPT, ops.message())
            .await
            .expect("an operation update in time")
            .expect("a healthy stream")
            .expect("the stream ends only after the done operation");
        if op.done {
            return op;
        }
    }
}

/// Whether `ops` produces a done operation within [`QUIET`].
pub async fn done_within_quiet(ops: &mut Streaming<Operation>) -> bool {
    timeout(QUIET, done(ops)).await.is_ok()
}
