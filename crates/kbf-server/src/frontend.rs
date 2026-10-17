//! `kbf frontend`: the REAPI front end (`docs/design/architecture-v2.md`, sections 2, 7
//! and 9). It serves REAPI (Execute, the scheduler, operations), the operator API and
//! the worker listener's work streams, and holds no blob cache: a client's CAS,
//! ByteStream and action-cache calls are proxied to a storage node ([`kbf_front::Proxy`],
//! ByteStream passed through without holding a blob), and Execute and the scheduler
//! read and write the node through [`kbf_front::Backend::routed`] (the action cache,
//! the Action and Command blobs, the input check, the action-cache write of a result).
//!
//! **Storage is routed by the map.** With a state store the frontend keeps the members
//! and so the map, and with the TLS flags it can dial each member: every storage call it
//! makes is then routed by that map ([`kbf_front::Routed`] over [`crate::front_maps`]),
//! once the map has two members, to the members the daemons place a blob on. Before that
//! (no member has registered, or one has), and without a state store or without the TLS
//! flags, every call goes to the one node `--storage` names. `--storage` is the bootstrap
//! and the farm of one, never one of several: blobs written to it before the map had two
//! members are not moved when the map places them elsewhere. Each member is dialled over
//! mutual TLS with the same client identity as `--storage`, and verified against the host
//! of the address it registered with.
//!
//! **The flags** are `kbf-server`'s own for what a frontend serves (the listeners, their
//! TLS, the REAPI policy, the scheduler's pacing, the operator API, `--state-store`, the
//! logs and the thread pools: [`ALLOWED`] and the thread flags), and its own for how it
//! reaches storage: `--storage` (required) and, together, `--storage-client-cert`,
//! `--storage-client-key` and `--storage-ca`. Every other flag of `kbf-server` is
//! refused by name ([`parse`]), computed from [`Args`] as `kbf storage` does it: the
//! local stores, the write tier, the segment uploads and the input-tree cache are a
//! storage node's, and a flag added to [`Args`] that is not listed here is refused with
//! no edit.
//!
//! **Storage is reached over mutual TLS.** A storage node refuses plain text off
//! loopback, so with the three flags the frontend dials `https://` and presents the
//! certificate (which must name the role `frontend`: `urn:kbf:role:frontend`), and
//! verifies the node's certificate against the CA and against the host in the `--storage`
//! URL. Both paths use one connection, so the proxy's raw HTTP/2 path and the tonic
//! calls cannot differ. Without them the URL must be `http://` to a loopback host, for
//! tests and one machine; [`parse`] refuses a plain-text URL to any other host.
//!
//! **The state store** is optional, as `kbf-server`'s is (`--state-store`): without it
//! the scheduler's state is in memory and the start logs the same warning. It becomes
//! required when the backends do (the design's deploy shapes all have one); a flag
//! given is opened, probed by `/readyz` and refused at start if this build has no
//! backend for it.
//!
//! **Readiness** (`GET /readyz`, `grpc.health.v1`) fails when the storage node does not
//! answer an action-cache lookup within `--readyz-store-timeout-ms`, when the state store (if
//! configured) does not answer a read, and when the storage map is not loaded (always
//! loaded today: [`crate::health::MapProbe`]). The storage check asks the `--storage`
//! node until the frontend routes by the map, and then the owners the map names for a
//! probe digest (it passes when one answers), so a farm that has lost its `--storage`
//! node is ready while its other members are.
//!
//! **Not served here:** the worker listener's `ByteStream`, CAS and `Segments` (a
//! daemon's blob traffic; daemons reach storage directly once the map reaches them,
//! av2-08), the build pages and the blob route of the operator API (they read a local
//! cache), and the input-tree memory of the cache (the walk of an Execute's inputs reads
//! the node's directories each time).

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use clap::{Arg, ArgMatches, Command, FromArgMatches as _};
use kbf_auth::AuthenticateLayer;
use kbf_front::{
    Backend, MAX_MESSAGE_BYTES, Owner, Owners, Proxy, RECORDER_CHANNEL, Recorder,
    RequestMetadataLayer, Routed,
};
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::worker::worker_server::WorkerServer;
use kbf_route::TlsNodes;
use kbf_storage::Storage;
use kbf_storage::service::RemoteStorage;
use kbf_threads::Topology;
use kbf_tls::{Incoming, TlsSource};
use tonic::service::Routes;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server};

use crate::drain::Drain;
use crate::farm::Farm;
use crate::front_maps::FrontMaps;
use crate::grpc_health::{HealthService, WATCH_INTERVAL};
use crate::health::{Checks, MapProbe, Readiness, StateProbe, StorageProbe};
use crate::members::Members;
use crate::serve::{
    AbortOnDrop, ServeError, bind, bind_api, drain_and_stop, flow_control, keepalive, log_state,
    map_of, members_of, membership, serve_api, stop_api, sweeping, worker_peers,
};
use crate::start::{Escalation, escalating, interruptions};
use crate::worker::WorkerService;
use crate::{Args, ConfigError, SERVER_VERSION, sizes_on};

/// The flags of [`Args`] a frontend takes, besides the thread flags
/// ([`kbf_threads::PoolArgs`]): what it listens on and how it schedules.
pub const ALLOWED: &[&str] = &[
    "listen",
    "reapi_tls_cert",
    "reapi_tls_key",
    "reapi_auth_policy",
    "storage",
    "worker_listen",
    "api_listen",
    "api_token_file",
    "readyz_store_timeout_ms",
    "worker_tls_cert",
    "worker_tls_key",
    "worker_client_ca",
    "tls_reload_interval_secs",
    "worker_deny_list",
    "heartbeat_interval_ms",
    "progress_interval_secs",
    "http2_keepalive_interval_secs",
    "http2_keepalive_timeout_secs",
    "reapi_max_streams",
    "worker_max_streams",
    "unservable_wait_secs",
    "memory_ladder_gib",
    "finished_retention_secs",
    "shutdown_timeout_secs",
    "invocation_memory",
    "state_store",
    "member_ttl_secs",
    "member_dwell_secs",
    "log_dir",
    "log_max_bytes",
    "log_files",
];

/// The ids of the three flags that give the frontend its client identity toward storage,
/// in the order [`FrontendFlags::tls`] reads them.
const TLS: [&str; 3] = ["storage_client_cert", "storage_client_key", "storage_ca"];

/// The frontend's flags, from one command line.
#[derive(Clone, Debug)]
pub struct FrontendFlags {
    /// The flags of `kbf-server` the frontend takes; every other field of it is its
    /// default and unused.
    pub server: Args,
    /// The storage node `--storage` names, checked against the TLS flags.
    pub storage: Endpoint,
    /// The client certificate, its key and the CA the node's certificate chains to, all
    /// three or none.
    pub tls: Option<[PathBuf; 3]>,
}

/// The command line of `kbf frontend`: [`Args`]'s, those it takes shown and the rest
/// hidden (so [`parse`] can refuse them by name), `--storage` required, and the three
/// flags toward storage.
#[must_use]
pub fn command() -> Command {
    let path =
        |id: &'static str, long: &'static str, requires: [&'static str; 2], help: &'static str| {
            Arg::new(id)
                .long(long)
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires_all(requires)
                .help(help)
        };
    crate::flag_set::command(
        "frontend",
        "The REAPI front end: schedules actions on daemons and proxies blobs to storage.",
        ALLOWED,
    )
    .mut_arg("storage", |a| a.required(true))
    .arg(path(
        TLS[0],
        "storage-client-cert",
        [TLS[1], TLS[2]],
        "The frontend's certificate toward the storage node (PEM); it must name the role \
         `frontend` (a URI subjectAltName urn:kbf:role:frontend).",
    ))
    .arg(path(
        TLS[1],
        "storage-client-key",
        [TLS[0], TLS[2]],
        "The private key of --storage-client-cert (PEM).",
    ))
    .arg(path(
        TLS[2],
        "storage-ca",
        [TLS[0], TLS[1]],
        "The CA the storage node's certificate chains to; the node must present a \
         certificate for the host in the --storage URL.",
    ))
}

/// The flags `m`, matched by [`command`], make.
///
/// # Errors
/// A flag of `kbf-server` that a frontend does not take was given (the error names it:
/// a frontend holds no local store and takes no write, segment or cache flag); `--storage`
/// is not a URL, is `https://` without the three TLS flags, has them and is not `https://`,
/// or is plain text to a host that is not loopback.
pub fn parse(m: &ArgMatches) -> Result<FrontendFlags, clap::Error> {
    let refuse = |why: String| command().error(clap::error::ErrorKind::ArgumentConflict, why);
    if let Some(flag) = crate::flag_set::refused(m, ALLOWED) {
        return Err(refuse(format!(
            "kbf frontend refuses --{flag}: a frontend holds no blobs and no cache of its own \
             (the stores, write tier, segment uploads and input-tree cache are a storage \
             node's); it takes the listener, scheduler, state-store and log flags \
             (docs/design/architecture-v2.md, section 2)"
        )));
    }
    let server = Args::from_arg_matches(m)?;
    let storage = server
        .storage()
        .map_err(|e| refuse(e.to_string()))?
        .expect("clap requires --storage");
    let paths: Vec<PathBuf> = TLS
        .iter()
        .filter_map(|id| m.get_one::<PathBuf>(id).cloned())
        .collect();
    // clap makes the three come together.
    let tls = <[PathBuf; 3]>::try_from(paths).ok();
    transport(&storage, tls.is_some()).map_err(refuse)?;
    Ok(FrontendFlags {
        server,
        storage,
        tls,
    })
}

/// Whether `url` and the presence of the TLS flags go together: mutual TLS to an
/// `https://` URL, or plain text to a loopback host.
fn transport(url: &Endpoint, tls: bool) -> Result<(), String> {
    let (scheme, host) = (url.uri().scheme_str(), url.uri().host().unwrap_or_default());
    let secure = scheme == Some("https");
    if tls != secure {
        return Err(format!(
            "--storage {url:?} and the TLS flags disagree: mutual TLS to storage needs an \
             https:// URL together with --storage-client-cert, --storage-client-key and \
             --storage-ca, and plain text is http://",
            url = url.uri().to_string()
        ));
    }
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(crate::config::loopback_only);
    if !tls && !loopback {
        return Err(format!(
            "--storage {url:?} is plain text to a host that is not loopback, and a storage \
             node refuses plain text there: give https:// and --storage-client-cert, \
             --storage-client-key and --storage-ca",
            url = url.uri().to_string()
        ));
    }
    Ok(())
}

impl FrontendFlags {
    /// The CA and the client identity the TLS flags name, or `None` without them.
    ///
    /// # Errors
    /// A TLS file cannot be read.
    pub fn storage_identity(&self) -> Result<Option<(Certificate, Identity)>, ConfigError> {
        let Some([cert, key, ca]) = &self.tls else {
            return Ok(None);
        };
        Ok(Some((
            Certificate::from_pem(crate::config::read(ca)?),
            Identity::from_pem(crate::config::read(cert)?, crate::config::read(key)?),
        )))
    }

    /// The connection to the storage node: lazy, with the client identity and the CA
    /// when the TLS flags are given, the node's certificate checked against the host of
    /// the URL.
    ///
    /// # Errors
    /// A TLS file cannot be read, or the transport refuses the configuration.
    pub fn storage_endpoint(&self) -> Result<Endpoint, ConfigError> {
        let Some((ca, identity)) = self.storage_identity()? else {
            return Ok(self.storage.clone());
        };
        let tls = ClientTlsConfig::new().ca_certificate(ca).identity(identity);
        self.storage
            .clone()
            .tls_config(tls)
            .map_err(|source| ConfigError::Storage {
                url: self.storage.uri().to_string(),
                source,
            })
    }
}

/// The frontend's own storage routing: the members' map and a connection to each member,
/// presenting the identity of the TLS flags. `None` (the node `--storage` names serves
/// everything) without a state store, which is where the members and their map live, and
/// without the TLS flags, which every connection to a member needs: a member is dialled
/// at `https://` and verified against its own address. With a state store but no TLS flags
/// the start logs a warning that says so, since the farm may have members the daemons
/// place blobs on that this frontend will not look at. Not generic, for the same reason
/// as [`crate::serve::sweeping`].
///
/// # Errors
/// A TLS file cannot be read.
fn routed_of(
    members: Option<&Arc<Members>>,
    flags: &FrontendFlags,
    storage: &RemoteStorage,
) -> Result<Option<Arc<Routed>>, ConfigError> {
    let Some(members) = members else {
        return Ok(None);
    };
    let Some((ca, identity)) = flags.storage_identity()? else {
        tracing::warn!(
            "a state store is configured but the TLS flags are not: members cannot be dialled, \
             so every storage call goes to the --storage node alone, and blobs the daemons \
             place on other members by the map will not be found"
        );
        return Ok(None);
    };
    let maps = Arc::new(FrontMaps::new(Arc::clone(members)));
    Ok(Some(Arc::new(
        Routed::over_tls(maps, Arc::new(TlsNodes::new(ca, identity))).bootstrapped(storage.clone()),
    )))
}

/// A started frontend: bound, not yet serving.
pub struct FrontendHandle {
    /// Where the REAPI listener is.
    pub reapi: SocketAddr,
    /// Where the worker listener is.
    pub worker: SocketAddr,
    /// Where the operator API listener is, if there is one.
    pub api: Option<SocketAddr>,
    drain: Arc<Drain>,
    /// Completes on the second stop signal; [`FrontendHandle::serve`] then closes the
    /// drain ([`Drain::close`]).
    escalation: Option<Escalation>,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
}

impl FrontendHandle {
    /// The frontend's drain ([`Drain`]): `POST /v1/frontend/drain` begins it, and the
    /// shutdown future given to [`start_frontend`] does.
    #[must_use]
    pub fn drain(&self) -> Arc<Drain> {
        Arc::clone(&self.drain)
    }

    /// The line the frontend prints once it is bound:
    /// `kbf-frontend <version> reapi=<addr> worker=<addr>[ api=<addr>] storage=<url>`.
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// Serves every listener until the shutdown future given to [`start_frontend`]
    /// completes and the open Execute streams finish or `--shutdown-timeout-secs` runs
    /// out (the drain `kbf-server` runs: [`crate::serve::drain_and_stop`]).
    ///
    /// # Errors
    /// A listener fails while serving.
    pub async fn serve(self) -> Result<(), ServeError> {
        escalating(self.serving, &self.drain, self.escalation).await
    }
}

/// What `/readyz` and the health service ask of a frontend, in the order they list the
/// failures: the stop signal and the scheduler role ([`Readiness`]), the storage (the
/// `--storage` node, or the owners the map names once `routed` routes by it), the state
/// store if there is one, and the storage map.
#[must_use]
pub fn checks(
    readiness: Arc<Readiness>,
    storage: Arc<dyn Storage>,
    routed: Option<Arc<Routed>>,
    state_store: Option<Arc<dyn kbf_state::Store>>,
    timeout: Duration,
) -> Checks {
    let probe = StorageProbe::new(storage, timeout);
    let probe = match routed {
        Some(routed) => probe.routed(routed),
        None => probe,
    };
    let mut checks = Checks::new(readiness).probing("storage", Arc::new(probe));
    if let Some(store) = state_store {
        checks = checks.probing("state_store", Arc::new(StateProbe::new(store, timeout)));
    }
    checks.probing("map", Arc::new(MapProbe))
}

/// What a frontend serves until: the shutdown future [`start_frontend`]'s caller makes,
/// made once everything that can be refused has been (and so before anything is bound).
type Shutdown = Box<dyn FnOnce() -> io::Result<Pin<Box<dyn Future<Output = ()> + Send>>>>;

/// Binds the frontend the flags describe. Nothing is opened but the state store (if
/// given), and the storage node is dialed when first used.
///
/// `shutdown` is called once, after the flags are checked and before anything is bound,
/// and returns the future whose completion stops the frontend; the binary's is
/// [`interrupted`].
///
/// # Errors
/// A flag the frontend refuses, a TLS file that cannot be read, a state store that
/// cannot be opened, a listener that cannot be bound, or a `shutdown` that fails.
pub fn start_frontend<S, F>(
    flags: &FrontendFlags,
    shutdown: S,
) -> Result<FrontendHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F> + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    // No branch here: this is the one generic function, and `bind_frontend` is the code.
    bind_frontend(
        flags,
        Box::new(move || {
            shutdown().map(|f| Box::pin(f) as Pin<Box<dyn Future<Output = ()> + Send>>)
        }),
    )
}

/// [`start_frontend`], not generic over the shutdown future: every test's start counts
/// toward the same function's coverage.
fn bind_frontend(
    flags: &FrontendFlags,
    shutdown: Shutdown,
) -> Result<FrontendHandle, Box<dyn Error>> {
    let args = &flags.server;
    let listeners = args.listeners()?;
    let policy = args.reapi_auth_policy()?;
    let api = args.api()?;
    let channel = flags.storage_endpoint()?.connect_lazy();
    let state = crate::start::with_members(
        args.state_store
            .as_ref()
            .map(crate::StateStoreUri::open)
            .transpose()?,
        args.members(),
    );
    let members = members_of(state.as_ref());
    let map = map_of(None, members.as_ref())?;
    let readiness = Arc::new(Readiness::default());
    let timeout = listeners.store_probe_timeout;
    let storage = Arc::new(RemoteStorage::new(channel.clone(), 0));
    let state_store = store_of(state.as_ref());
    let routed = routed_of(members.as_ref(), flags, &storage)?;
    let checks = Arc::new(checks(
        Arc::clone(&readiness),
        storage.clone(),
        routed.clone(),
        state_store,
        timeout,
    ));
    let backend = match &routed {
        Some(routed) => Backend::routed(Arc::clone(routed), storage),
        None => Backend::remote(storage),
    };

    let (recorder, recording) = Recorder::new(listeners.invocation_memory, RECORDER_CHANNEL);
    let default_memory = listeners.memory_ladder.first();
    let durable = state.is_some();
    let farm = Farm::new(
        backend.clone(),
        listeners.unservable_wait,
        listeners.finished_retention,
    )
    .with_memory_ladder(listeners.memory_ladder.clone())
    .recording(recorder.clone());
    let farm = Arc::new(farm.with_state(state)?);
    log_state(durable, farm.term(), farm.lease_epoch());

    let (reapi_incoming, reapi) = bind(listeners.reapi)?;
    let (worker_incoming, worker) = bind(listeners.worker)?;
    let peers_certified = listeners.worker_tls.is_some();
    let (mut worker_server, peers, worker_tls) = worker_peers(
        keepalive(
            flow_control(Server::builder(), listeners.stream_limits.worker),
            listeners.streams,
        ),
        listeners.worker_tls,
    );
    let tls_sources: Vec<TlsSource> = listeners
        .reapi_tls
        .iter()
        .chain(&worker_tls)
        .cloned()
        .collect();
    let reapi_incoming = Incoming::new(reapi_incoming, listeners.reapi_tls);
    let worker_incoming = Incoming::new(worker_incoming, worker_tls);
    let api_listener = api.as_ref().map(|api| bind_api(api.listen)).transpose()?;
    let api_addr = api_listener.as_ref().map(|(_, local)| *local);
    let (closer, closing) = kbf_front::closing();
    let (drain, goaway) = Drain::new(closer, Arc::clone(&readiness));
    let drain = Arc::new(drain);
    let api_routes = crate::api::router(
        Arc::clone(&farm),
        api.and_then(|api| api.token),
        Arc::clone(&drain),
        members.clone(),
    )
    .merge(crate::health::router_of(Arc::clone(&checks)))
    .merge(metrics_route(backend.clone(), tls_sources));

    let mut reapi_server = keepalive(
        flow_control(Server::builder(), listeners.stream_limits.reapi),
        listeners.streams,
    );
    let sessions = WorkerService::new(
        Arc::clone(&farm),
        peers,
        listeners.heartbeat_interval,
        listeners.hello_wait,
    );
    // Daemons are sent the members' map: Welcome.map and a MapUpdate at each bump.
    let sessions = map.into_iter().fold(sessions, WorkerService::with_map);
    let worker_service = WorkerServer::new(sessions)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);

    let membership = membership(members.clone(), peers_certified);
    let (ticker, progress) = kbf_front::progress(listeners.streams.progress_interval);
    let authorizers = Arc::new(policy.authorizers);
    let bootstrap = Owner::new(&flags.storage.uri().to_string(), channel);
    let owners = match routed {
        Some(routed) => Owners::routed(routed, bootstrap),
        None => Owners::single(bootstrap),
    };
    let proxy = Proxy::new(owners, Arc::clone(&authorizers));
    let reapi_routes = kbf_front::routes_proxied(
        &proxy,
        backend,
        Arc::clone(&farm),
        kbf_front::OpenStreams { closing, progress },
        authorizers,
        &recorder,
        default_memory,
    );
    let reapi_routes = Routes::from(
        reapi_routes
            .into_axum_router()
            .layer(AuthenticateLayer::new(policy.authenticator))
            .layer(RequestMetadataLayer::new()),
    )
    .add_service(HealthServer::new(HealthService::of(
        Arc::clone(&checks),
        WATCH_INTERVAL,
    )));

    let shutdown = shutdown()?;
    let stopper = Arc::clone(&drain);
    let serving = async move {
        let _recording = AbortOnDrop(tokio::spawn(recording.run()).abort_handle());
        let reapi_serve = reapi_server
            .add_routes(reapi_routes)
            .serve_with_incoming_shutdown(reapi_incoming, async {
                let _ = goaway.await;
            });
        let worker_serve = worker_server
            .add_service(worker_service)
            .add_optional_service(membership)
            .serve_with_incoming(worker_incoming);
        // Told when the REAPI listener is done (see `serve::stop_api`).
        let (api_stop, api_stopped) = tokio::sync::oneshot::channel::<()>();
        let api_serve = serve_api(
            api_listener.map(|(listener, _)| listener),
            api_routes,
            async {
                let _ = api_stopped.await;
            },
        );
        tokio::pin!(api_serve);
        let sweeping = sweeping(members);
        let ticking = async {
            let mut tick = tokio::time::interval(listeners.tick);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                farm.tick().await;
            }
        };
        let stopping = async {
            shutdown.await;
            drain_and_stop(&stopper, listeners.shutdown_timeout).await;
        };
        let mut reapi_done = false;
        let outcome = tokio::select! {
            served = reapi_serve => {
                reapi_done = true;
                served.map_err(ServeError::from)
            }
            served = worker_serve => served.map_err(ServeError::from),
            served = &mut api_serve => served.map_err(ServeError::Api),
            () = sweeping => Ok(()),
            () = ticking => Ok(()),
            () = ticker.run() => Ok(()),
            () = stopping => Ok(()),
        };
        if reapi_done {
            stop_api(api_stop, api_serve).await;
        }
        outcome
    };
    let plain = args.plain_text_off_loopback();
    Ok(FrontendHandle {
        reapi,
        worker,
        api: api_addr,
        drain,
        escalation: None,
        start_line: start_line(
            reapi,
            worker,
            api_addr,
            &flags.storage.uri().to_string(),
            &plain,
        ),
        serving: Box::pin(serving),
    })
}

/// The store `/readyz` probes: the open state store's, if there is one.
fn store_of(state: Option<&crate::StateStore>) -> Option<Arc<dyn kbf_state::Store>> {
    state.map(|state| Arc::clone(&state.store))
}

/// `GET /metrics`: what the service counts, in the Prometheus text format.
fn metrics_route(backend: Backend, tls: Vec<TlsSource>) -> axum::Router {
    let tls = Arc::new(tls);
    let render = move || {
        let (backend, tls) = (backend.clone(), Arc::clone(&tls));
        async move {
            let mut text = backend.render_metrics();
            kbf_tls::render_metrics(tls.iter(), &mut text);
            ([(CONTENT_TYPE, crate::metrics::TEXT_FORMAT)], text)
        }
    };
    axum::Router::new().route("/metrics", get(render))
}

/// The start line, with one warning naming the listeners in `plain` when there are any.
fn start_line(
    reapi: SocketAddr,
    worker: SocketAddr,
    api: Option<SocketAddr>,
    storage: &str,
    plain: &[&str],
) -> String {
    let api = api.map(|a| format!(" api={a}")).unwrap_or_default();
    let warning = if plain.is_empty() {
        String::new()
    } else {
        format!(" warning=plaintext-off-loopback:{}", plain.join(","))
    };
    format!(
        "kbf-frontend {SERVER_VERSION} reapi={reapi} worker={worker}{api} storage={storage}{warning}"
    )
}

/// `kbf frontend`: the logs, a runtime the thread flags size, the start line on stdout,
/// and serving until SIGINT or SIGTERM. Exit 2 for a start the frontend refuses.
#[must_use]
pub fn run(flags: &FrontendFlags) -> ExitCode {
    crate::log_file::init(&flags.server);
    match serve(flags) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf frontend: {e}");
            ExitCode::from(2)
        }
    }
}

fn serve(flags: &FrontendFlags) -> Result<(), Box<dyn Error>> {
    let sizes = sizes_on(&flags.server, Topology::detect());
    sizes.log();
    sizes.runtime()?.block_on(async {
        let (first, second) = interruptions()?;
        let mut frontend = start_frontend(flags, move || Ok(first))?;
        frontend.escalation = Some(second);
        println!("{}", frontend.start_line());
        Ok(frontend.serve().await?)
    })
}

#[cfg(test)]
mod tests;
