//! The server's assembly, as a library: [`start`] opens the store and metadata log the
//! flags name, attaches the write tier, starts the start-up pass over a `file://` cold
//! store in the background (`crate::startup_pass`) and binds the listeners, and returns
//! a [`ServerHandle`] that serves. The `kbf-server` binary is `start`, a print and
//! `serve`; a process that runs the server beside the worker daemon calls the same
//! function.
//!
//! [`serve_with`] is the seam under it: the same assembly over a Raft metadata log and
//! object store the caller opened, for a store kind the flags do not name.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use kbf_front::store_metrics::{Counted, StoreMetrics};
use kbf_front::{Cache, MetaLog};
use kbf_log::FarmLogId;
use kbf_meta::Retention;
use kbf_objstore::{
    Floor, FsStore, FsStoreError, KeyPrefix, ObjectStore, objects_source, statvfs_source,
};
use kbf_pack_index::PackIndex;
use kbf_storage::local_cache::{HeldBytes, LocalCache};
use kbf_store::StdFs;
use kbf_threads::Topology;
use kbf_threads::disk::Statvfs;
use kbf_tier::WriteTier;

use crate::drain::Drain;
use crate::local::LocalStore;
use crate::startup_pass::StartPass;
use crate::stores::{COLD_FILE_FLOOR, FILE_PREFIX_ROOT, Hot, StoreUri, Stores};
use crate::write_tier::FLUSH_EVERY;
use crate::{Args, RaftMetaLog, SERVER_VERSION, ServeError, bind_server_with_ui, s3_store};

/// The server's thread sizes on a given machine shape: every CPU, then the overrides.
pub fn sizes_on(args: &Args, topology: Topology) -> kbf_threads::Sizes {
    args.threads.resolve(topology)
}

/// Runs `serving`, and if `escalation` completes meanwhile (the second stop signal)
/// calls [`Drain::close`]: the open streams end UNAVAILABLE and the server stops within
/// a short grace. One function for `kbf-server` and `kbf frontend`.
pub(crate) async fn escalating<T>(
    serving: impl Future<Output = T>,
    drain: &Arc<Drain>,
    escalation: Option<Escalation>,
) -> T {
    let drain = Arc::clone(drain);
    let _escalation = escalation.map(|second| {
        AbortOnDrop(tokio::spawn(async move {
            second.await;
            tracing::warn!("a second stop signal: ending the open streams now");
            drain.close();
        }))
    });
    serving.await
}

/// The first stop signal's future and the second's, installed in one call: a signal
/// between the two handlers stops the server, and the second is then a third.
///
/// # Errors
/// A handler cannot be installed.
pub(crate) fn interruptions() -> io::Result<(impl Future<Output = ()> + Send + 'static, Escalation)>
{
    let first = interrupted()?;
    let second: Escalation = Box::pin(second_interrupt()?);
    Ok((first, second))
}

/// A future that completes on the second stop signal.
pub(crate) type Escalation = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A started server: bound, its write tier attached and its flusher (and any start-up
/// pass) running, not yet serving. [`ServerHandle::serve`] serves until the shutdown
/// future given to [`start`] completes; dropping the handle first stops the flusher and
/// the pass and releases the listeners.
pub struct ServerHandle {
    /// Where the REAPI listener is.
    pub reapi: SocketAddr,
    /// Where the worker listener is.
    pub worker: SocketAddr,
    /// Where the operator API listener is, if there is one.
    pub api: Option<SocketAddr>,
    local: LocalStore,
    drain: Arc<Drain>,
    /// Completes on the second stop signal ([`start_interruptible`]); [`serve`] then
    /// calls [`Drain::close`].
    ///
    /// [`serve`]: ServerHandle::serve
    escalation: Option<Escalation>,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
    _flusher: Option<AbortOnDrop>,
    _pass: Option<AbortOnDrop>,
    /// The pack index `--pack-index` opened, if the flag was given.
    pack_index: Option<Arc<dyn PackIndex>>,
}

impl ServerHandle {
    /// The pack index `--pack-index` opened on the Postgres state store, or `None` without the
    /// flag. Nothing in the server reads or writes it yet.
    #[must_use]
    pub fn pack_index(&self) -> Option<Arc<dyn PackIndex>> {
        self.pack_index.clone()
    }

    /// The line `kbf-server` prints once it is bound (see the binary's docs for its
    /// fields).
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// The server's cache as calls in this process: what a daemon run beside the server
    /// in one process reads and writes blobs through.
    #[must_use]
    pub fn local(&self) -> LocalStore {
        self.local.clone()
    }

    /// The frontend's drain ([`Drain`]): `POST /v1/frontend/drain` begins it, and the
    /// shutdown future given to [`start`] does. [`Drain::close`] ends the open streams
    /// now.
    #[must_use]
    pub fn drain(&self) -> Arc<Drain> {
        Arc::clone(&self.drain)
    }

    /// This handle with `second` as its escalation: when it completes while the handle
    /// serves, [`ServerHandle::serve`] calls [`Drain::close`], as the second stop signal of
    /// [`start_interruptible`] does. For a process that reads the stop signals itself, to
    /// hand them to a daemon beside the server as well, and passes the first to [`start`].
    #[must_use]
    pub fn escalate_on(mut self, second: impl Future<Output = ()> + Send + 'static) -> Self {
        self.escalation = Some(Box::pin(second));
        self
    }

    /// Serves every listener until the shutdown future ends and its connections close
    /// or `--shutdown-timeout-secs` runs out. A handle from [`start_interruptible`]
    /// also ends the open streams, and so the drain, at a second stop signal.
    ///
    /// # Errors
    /// A listener fails while serving.
    pub async fn serve(self) -> Result<(), ServeError> {
        let drain = self.drain();
        let Self {
            serving,
            escalation,
            _flusher,
            _pass,
            ..
        } = self;
        escalating(serving, &drain, escalation).await
    }
}

/// [`start`] with the binary's stop signals: SIGINT or SIGTERM stops the server
/// ([`interrupted`]), and a second one ends the drain that stop began
/// ([`ServerHandle::serve`] closes the open streams: they end UNAVAILABLE and the
/// server stops within a short grace). Both handlers are installed before anything is
/// bound.
///
/// # Errors
/// As [`start`].
pub async fn start_interruptible(args: &Args) -> Result<ServerHandle, Box<dyn Error>> {
    let slot: Arc<Mutex<Option<Escalation>>> = Arc::new(Mutex::new(None));
    let installing = Arc::clone(&slot);
    let mut handle = start(args, move || {
        let (first, second) = interruptions()?;
        *installing.lock().unwrap_or_else(PoisonError::into_inner) = Some(second);
        Ok(first)
    })
    .await?;
    handle.escalation = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    Ok(handle)
}

/// Builds the object store and the metadata log the flags name, binds the listeners
/// and attaches the write tier, as the `kbf-server` binary does before it prints its
/// start line.
///
/// `shutdown` is called once, after the stores are open and before anything is bound,
/// and returns the future whose completion stops the server; the binary's is
/// [`interrupted`]. A process that runs the server beside another service passes its
/// own.
///
/// # Errors
/// A flag the server refuses, a store or log that cannot be opened, a listener that
/// cannot be bound, a write tier that refuses to start, or a `shutdown` that fails.
pub async fn start<S, F>(args: &Args, shutdown: S) -> Result<ServerHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    args.check_scheduler_core()?;
    let stores = args.stores(&Statvfs)?;
    for line in stores.describe() {
        tracing::info!("{line}");
    }
    match &stores.cold {
        StoreUri::File(dir) => {
            let store = open_cold_file(args, dir)?;
            let files = store.clone();
            over(
                args,
                &stores,
                (store, Some(files)),
                FILE_PREFIX_ROOT,
                shutdown,
            )
            .await
        }
        StoreUri::S3(uri) => {
            let store = s3_store(uri, |name| std::env::var(name).ok())?;
            over(args, &stores, (store, None), &uri.prefix, shutdown).await
        }
    }
}

/// A `file://` cold store: its floor [`COLD_FILE_FLOOR`], or for tests
/// `--cold-store-floor-bytes`; its filesystem the real one, or for tests one of
/// `--cold-store-size-bytes`.
pub(crate) fn open_cold_file(args: &Args, dir: &Path) -> Result<FsStore, FsStoreError> {
    let floor = args
        .cold_store_floor_bytes
        .map_or(COLD_FILE_FLOOR, Floor::Bytes);
    let space = args
        .cold_store_size_bytes
        .map_or_else(statvfs_source, objects_source);
    FsStore::open_with_space(dir, floor, space)
}

/// Serves `store` under the Raft metadata log in `stores.meta_dir`, writing under the
/// prefix it committed: `<prefix_root><log id>/` for a new log. With `files`, the same
/// store as a file store (a `file://` cold store), the start-up pass runs over it. A
/// `file://` hot store gets none: it is the write tier's block files, not an object
/// store, and the tier's own start-up reads them.
async fn over<O, S, F>(
    args: &Args,
    stores: &Stores,
    (store, files): (O, Option<FsStore>),
    prefix_root: &str,
    shutdown: S,
) -> Result<ServerHandle, Box<dyn Error>>
where
    O: ObjectStore + 'static,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let log = RaftMetaLog::open_dir(&stores.meta_dir, Retention::default(), prefix_root).await?;
    let (prefix, id) = (log.store_prefix(), log.log_id());
    let grace = Duration::from_millis(args.cold_store_evict_grace_ms);
    let pass = files.map(|files| StartPass::new(files, grace));
    let hot = stores.hot.as_ref();
    assemble(args, hot, log, store, prefix, id, shutdown, pass).await
}

/// [`start`] over a metadata log and object store already open: where the flags'
/// store kinds end and the rest of the assembly (cache, write tier, flusher, listeners)
/// begins. `hot` is the resolved hot store, if any; `prefix` is where objects are
/// written and `log` the Raft log's id, for the start line.
///
/// # Errors
/// As [`start`], but for opening the store and log.
pub async fn serve_with<O, S, F>(
    args: &Args,
    hot: Option<&Hot>,
    meta: RaftMetaLog<StdFs>,
    store: O,
    prefix: KeyPrefix,
    log: FarmLogId,
    shutdown: S,
) -> Result<ServerHandle, Box<dyn Error>>
where
    O: ObjectStore + 'static,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    args.check_scheduler_core()?;
    assemble(args, hot, meta, store, prefix, log, shutdown, None).await
}

/// [`serve_with`], and `pass` spawned once the write tier is attached: it runs beside
/// the server and is never awaited here.
#[expect(clippy::too_many_arguments, reason = "serve_with's, and the pass")]
pub(crate) async fn assemble<O, S, F>(
    args: &Args,
    hot: Option<&Hot>,
    meta: RaftMetaLog<StdFs>,
    store: O,
    prefix: KeyPrefix,
    log: FarmLogId,
    shutdown: S,
    pass: Option<StartPass>,
) -> Result<ServerHandle, Box<dyn Error>>
where
    O: ObjectStore + 'static,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    args.retired_release.warn();
    let listeners = args.listeners()?;
    let policy = args.reapi_auth_policy()?;
    let pack_index = args.open_pack_index()?;
    let state = with_members(
        args.open_state_store().map_err(ServeError::State)?,
        args.members(),
    );
    let state = crate::history::with_history(state, &args.history)?;
    let routes = Routes {
        invocations: policy.invocations_get.is_some(),
        blobs: policy.invocations_get.is_some() && policy.blobs_get.is_some(),
    };
    // Every request to the cold store is counted into the cache's scrape.
    let cold = Arc::new(StoreMetrics::cold());
    let store = Counted::new(store, Arc::clone(&cold));
    let cache = Cache::open(meta, store, prefix)
        .await?
        .with_cold_store_metrics(cold)
        .with_writes(args.write_config())
        .with_write_pool(args.write_pool_config())
        .with_durable_wait(args.durable_wait())
        .with_input_trees(args.input_tree_config());
    let local = args.local_cache.as_ref();
    let (cache, tier) = crate::write_tier::attach_with(cache, hot, local.is_none()).await?;
    let cache = Arc::new(cache);
    share_storage(local, tier_bytes(Arc::downgrade(&cache)));
    let local = LocalStore::new(Arc::clone(&cache), listeners.segments);
    let _flusher = tier.map(|_| {
        let flusher = crate::write_tier::run_flusher(Arc::clone(&cache), FLUSH_EVERY);
        AbortOnDrop(tokio::spawn(flusher))
    });
    let _pass = pass.map(|pass| AbortOnDrop(crate::startup_pass::spawn(Arc::clone(&cache), pass)));
    let shutdown = shutdown()?;
    let api = args.api()?;
    let storage = args.storage()?;
    let proxy = args.proxy()?;
    let bound = bind_server_with_ui(
        cache, listeners, api, policy, state, storage, None, proxy, args.ui, shutdown,
    )?;
    let plain = args.plain_text_off_loopback();
    Ok(ServerHandle {
        reapi: bound.reapi,
        worker: bound.worker,
        api: bound.api,
        local,
        drain: bound.drain,
        escalation: None,
        start_line: start_line(
            bound.reapi,
            bound.worker,
            bound.api,
            routes,
            Some(log),
            &plain,
        ),
        serving: Box::pin(bound.serving),
        _flusher,
        _pass,
        pack_index,
    })
}

/// Reports local storage's `bytes` to the node's one cache, if the process has one.
/// Not generic, so it is compiled once whatever the store.
pub(crate) fn share_storage(local: Option<&Arc<LocalCache>>, bytes: HeldBytes) {
    if let Some(local) = local {
        local.attach_storage(bytes);
    }
}

/// What `cache`'s write tier holds on its disk now: its blocks, and any flushed copies
/// it keeps; 0 without a tier or once the server is gone (a weak handle, as the input
/// cache's side is: the one cache keeps neither half alive).
pub(crate) fn tier_bytes<M: MetaLog, O: ObjectStore + 'static>(
    cache: Weak<Cache<M, O>>,
) -> HeldBytes {
    Box::new(move || {
        cache
            .upgrade()
            .map_or(0, |cache| cache.write_tier().map_or(0, held))
    })
}

/// What `tier` holds on its disk.
fn held(tier: &dyn WriteTier) -> u64 {
    let usage = tier.usage();
    usage.used.saturating_add(usage.flushed)
}

/// Aborts a task when dropped: the flusher and the start-up pass stop with the server.
pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Which of the operator API's routes that are off unless the policy names them are on.
#[derive(Clone, Copy, Debug)]
struct Routes {
    /// The build pages, `GET /v1/invocations/{id}` and below.
    invocations: bool,
    /// `GET /v1/blobs/{digest}`.
    blobs: bool,
}

/// `on` or `off`.
const fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The start line: the version, the addresses bound (with the operator API's, whether
/// it serves the build pages and `blobs`) and the Raft log's id, and one warning
/// naming the listeners in `plain` (those serving plain text off loopback) when there
/// are any.
fn start_line(
    reapi: SocketAddr,
    worker: SocketAddr,
    api: Option<SocketAddr>,
    routes: Routes,
    log: Option<FarmLogId>,
    plain: &[&str],
) -> String {
    let (invocations, blobs) = (on_off(routes.invocations), on_off(routes.blobs));
    let api = api
        .map(|a| format!(" invocations={invocations} blobs={blobs} api={a}"))
        .unwrap_or_default();
    let log = log.map(|id| format!(" log={id}")).unwrap_or_default();
    let warning = if plain.is_empty() {
        String::new()
    } else {
        format!(" warning=plaintext-off-loopback:{}", plain.join(","))
    };
    format!("kbf-server {SERVER_VERSION} reapi={reapi} worker={worker}{api}{log}{warning}")
}

/// Installs the SIGINT and SIGTERM handlers now and returns a future that completes on
/// either signal.
///
/// `tokio::signal::ctrl_c()` installs its handler only when first polled, which is
/// after the start line is printed; a SIGINT in between killed the process by the
/// default action instead of stopping it cleanly (issue #86). `signal` installs the
/// handler when it is called.
///
/// # Errors
/// The handler cannot be installed.
#[cfg(unix)]
pub fn interrupted() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    })
}

/// Installs the SIGINT and SIGTERM handlers now and returns a future that completes on
/// the second signal of either kind (a second SIGTERM, SIGINT, or one of each), which
/// [`start_interruptible`] hands to [`ServerHandle::serve`] to end the drain a first
/// signal began. Two signals that arrive before the future is first polled count as one.
///
/// # Errors
/// The handler cannot be installed.
#[cfg(unix)]
fn second_interrupt() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    Ok(async move {
        for _ in 0..2 {
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
        }
    })
}

/// Returns a future that completes on the second Ctrl-C. Elsewhere than Unix the
/// handler is installed when the future is first polled.
#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps, reason = "the same signature as on Unix")]
fn second_interrupt() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    Ok(async {
        for _ in 0..2 {
            let _ = tokio::signal::ctrl_c().await;
        }
    })
}

/// Returns a future that completes on Ctrl-C. Elsewhere than Unix the handler is
/// installed when the future is first polled, after the start line is printed.
#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps, reason = "the same signature as on Unix")]
pub fn interrupted() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

/// The state store with the membership timing the flags name.
pub(crate) fn with_members(
    state: Option<crate::StateStore>,
    members: crate::members::MembershipConfig,
) -> Option<crate::StateStore> {
    state.map(|state| crate::StateStore { members, ..state })
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use kbf_state::ManualClock;
    use kbf_state::fake::{Fake, Flaw};

    use super::*;

    /// Catches `--member-ttl-secs`/`--member-dwell-secs` parsed and then dropped at
    /// start: the store a start opens must carry the flags' timing, keep its other
    /// fields, and a start without a state store must stay without one.
    #[test]
    fn with_members_sets_the_timing_and_keeps_the_rest() {
        let members = crate::members::MembershipConfig {
            ttl: Duration::from_secs(9),
            dwell: Duration::from_secs(40),
            replication: 2,
        };
        let state = crate::StateStore {
            store: Arc::new(Fake::with_flaw(
                Arc::new(ManualClock::default()),
                Flaw::None,
            )),
            epoch: crate::farm::EpochPolicy::Keep,
            members: crate::members::MembershipConfig::default(),
            history: None,
        };
        let got = with_members(Some(state), members).expect("still a store");
        assert_eq!(got.members, members);
        assert_eq!(got.epoch, crate::farm::EpochPolicy::Keep);
        assert!(with_members(None, members).is_none());
    }

    /// Catches the public seam [`serve_with`] not assembling a server now that the
    /// flags' path ([`start`], which also attaches the start-up pass) goes around it:
    /// over a log and store its caller opened it binds, and its start line names the
    /// log.
    #[tokio::test(flavor = "current_thread")]
    async fn serve_with_binds_over_a_store_the_caller_opened() {
        let root = crate::test_dir::scratch_dir("kbf-server-serve-with");
        let args = Args::parse_from([
            "kbf-server",
            "--listen=127.0.0.1:0",
            "--worker-listen=127.0.0.1:0",
        ]);
        let log = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let (prefix, id) = (log.store_prefix(), log.log_id());
        let store = FsStore::open(&root.join("blobs"), Floor::Bytes(1)).expect("open the store");
        let pending = || Ok(std::future::pending::<()>());
        let server = serve_with(&args, None, log, store, prefix, id, pending)
            .await
            .expect("assembled");
        let line = server.start_line().to_owned();
        assert!(line.ends_with(&format!(" log={id}")), "{line}");
        drop(server);
    }

    /// Catches: `--state-store` accepted by the flags and then ignored at start (the
    /// scheduler would run in memory while the operator believes its state is kept): a
    /// start over a SQLite path makes the file and serves, a second start over it while
    /// the first runs is refused (two frontends on one file), and a store that cannot be
    /// opened stops the start before anything is served.
    #[tokio::test(flavor = "current_thread")]
    async fn serve_with_opens_the_state_store_and_refuses_a_second_frontend() {
        let root = crate::test_dir::scratch_dir("kbf-server-state-flag");
        let state = root.join("state").join("state.db");
        let args = Args::parse_from([
            "kbf-server".to_owned(),
            "--listen=127.0.0.1:0".to_owned(),
            "--worker-listen=127.0.0.1:0".to_owned(),
            format!("--state-store=sqlite://{}", state.display()),
        ]);
        let start = |name: &'static str| {
            let (args, root) = (&args, &root);
            async move {
                let log = RaftMetaLog::open_dir(&root.join(name), Retention::default(), "kbf/")
                    .await
                    .expect("open the log");
                let (prefix, id) = (log.store_prefix(), log.log_id());
                let store = FsStore::open(&root.join(format!("{name}-blobs")), Floor::Bytes(1))
                    .expect("open the store");
                serve_with(args, None, log, store, prefix, id, interrupted).await
            }
        };
        let first = start("one").await.expect("the first frontend starts");
        assert!(state.exists(), "the state file is made");
        let refused = start("two").await.err().expect("the file is held");
        let said = refused.to_string();
        assert!(said.starts_with("--state-store: "), "{said}");
        assert!(said.contains("another kbf process"), "{said}");
        drop(first);
        start("three").await.expect("free once the first is gone");
    }

    /// Starts a server over `args` on a metadata log and a cold store of its own, in the
    /// directory `dir` under `root`.
    async fn boot(args: &Args, root: &Path, dir: &str) -> Result<ServerHandle, Box<dyn Error>> {
        let log = RaftMetaLog::open_dir(&root.join(dir), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let (prefix, id) = (log.store_prefix(), log.log_id());
        let store = FsStore::open(&root.join(format!("{dir}-blobs")), Floor::Bytes(1))
            .expect("open the store");
        serve_with(args, None, log, store, prefix, id, interrupted).await
    }

    /// Asserts that `--pack-index` reaches the started server: over a Postgres state store a
    /// server given the flag carries the index and one without it carries none, and a start is
    /// refused with the line that says why when the flag has no state store or a SQLite one.
    #[tokio::test(flavor = "current_thread")]
    async fn a_started_server_carries_the_pack_index_the_flag_names() {
        // The test database makes and drops itself with a runtime of its own, which tokio
        // refuses on a thread that is running tasks: both happen on a thread of their own.
        let db = std::thread::spawn(kbf_pack_index::schema::test_database);
        let db = db.join().expect("a test database");
        let root = crate::test_dir::scratch_dir("kbf-server-pack-index");
        let mut args = Args::parse_from([
            "kbf-server",
            "--listen=127.0.0.1:0",
            "--worker-listen=127.0.0.1:0",
        ]);
        args.state_store = Some(db.url().parse().expect("a postgres URL"));
        let off = boot(&args, &root, "off").await;
        let off = off.expect("starts without the flag");
        assert!(off.pack_index().is_none());
        drop(off);
        args.pack_index = true;
        let on = boot(&args, &root, "on").await;
        let on = on.expect("starts with the flag");
        assert!(on.pack_index().is_some());
        drop(on);
        let file = root.join("state.db");
        args.state_store = Some(crate::StateStoreUri::Sqlite(file));
        let refused = boot(&args, &root, "sqlite").await;
        let said = refused.err().expect("SQLite").to_string();
        assert!(said.contains("not SQLite"), "{said}");
        args.state_store = None;
        let refused = boot(&args, &root, "none").await;
        let said = refused.err().expect("no state store").to_string();
        assert!(said.contains("needs --state-store"), "{said}");
        let dropped = std::thread::spawn(move || drop(db));
        dropped.join().expect("the test database is dropped");
    }

    /// Catches the server sizing itself from anything but the CPU count: on the 44-core,
    /// 2-threads-per-core farm node it runs 88 async workers, 512 blocking threads
    /// (tokio's cap) and 88 CPU threads, on a 3-CPU machine 3, 512 and 3; a flag still
    /// wins over its default (alone), and `--reserve-cpus` and `--reserve-share` are
    /// refused, so a stale unit file is noticed.
    #[test]
    fn the_server_uses_every_cpu() {
        let triple = |args: &Args, topology| {
            let sizes = sizes_on(args, topology);
            (
                sizes.async_threads(),
                sizes.blocking_threads(),
                sizes.cpu_threads(),
            )
        };
        let args = Args::parse_from(["kbf-server"]);
        assert_eq!(triple(&args, Topology::new(88, 2)), (88, 512, 88));
        assert_eq!(triple(&args, Topology::new(3, 1)), (3, 512, 3));
        assert_eq!(args.cpu_threads_on(Topology::new(88, 2)).get(), 88);
        let args = Args::parse_from(["kbf-server", "--blocking-threads=9"]);
        assert_eq!(triple(&args, Topology::new(88, 2)), (88, 9, 88));
        let args = Args::parse_from(["kbf-server", "--async-threads=5", "--cpu-threads=6"]);
        assert_eq!(triple(&args, Topology::new(88, 2)), (5, 512, 6));
        assert_eq!(args.cpu_threads_on(Topology::new(88, 2)).get(), 6);
        for flag in ["--reserve-cpus=16", "--reserve-share=2"] {
            assert!(
                Args::try_parse_from(["kbf-server", flag]).is_err(),
                "{flag}"
            );
        }
    }
}
