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
//! node is ready while its other members are. `READY=1` to the service manager, and the
//! successor's "serving" to its predecessor, do not wait for this check
//! (`handover::Ready`); `/readyz` stays failing until storage answers.
//!
//! **Handing over** (`--handover-socket` with `--instance`, [`crate::handover`]): behind
//! Envoy, a frontend started while another serves on the same state file binds its own
//! listeners (instance b's ports are [`Instance::SECOND_OFFSET`] above a's), takes the
//! other's state store and daemons, and serves; the other goes not ready once Envoy has
//! seen it, forwards the calls Envoy still sends it ([`kbf_front::Relay`]) and exits when
//! it holds nothing, without failing a build; [`start_or_take_over`] is its start.
//!
//! **Under the admin port** (`--admin-listen`, optionally `--await-takeover`,
//! [`crate::managed`]): a loopback port answers `GET role` and takes `quiesce`, `resume`,
//! `takeover` and `stand-down`, so a supervisor can start a candidate beside the serving
//! frontend and move the state store's generation to it without the Unix-socket handover
//! above (which stays until the supervisor replaces it).
//!
//! **Not served here:** the worker listener's `ByteStream`, CAS and `Segments` (a
//! daemon's blob traffic; daemons reach storage directly once the map reaches them,
//! av2-08), the build pages and the blob route of the operator API (they read a local
//! cache), and the input-tree memory of the cache (the walk of an Execute's inputs reads
//! the node's directories each time).

use std::error::Error;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use clap::{Arg, ArgMatches, Command, FromArgMatches as _};
use futures::FutureExt as _;
use futures::future::{AbortHandle, Abortable, BoxFuture};
use kbf_auth::AuthenticateLayer;
use kbf_front::{
    Backend, Handed, MAX_MESSAGE_BYTES, Owner, Owners, Proxy, RECORDER_CHANNEL, Recorder, Relaying,
    RequestMetadataLayer, Routed, Successor,
};
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::worker::worker_server::WorkerServer;
use kbf_route::TlsNodes;
use kbf_storage::Storage;
use kbf_storage::service::RemoteStorage;
use kbf_threads::Topology;
use kbf_tls::TlsSource;
use tonic::service::Routes;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server};

use crate::drain::Drain;
use crate::farm::Farm;
use crate::front_maps::FrontMaps;
use crate::grpc_health::{HealthService, WATCH_INTERVAL};
use crate::handover::{
    self, EnvoyCheck, Facts, Handover, HandoverError, Holder, Instance, Leave, Message, Peer,
    Ready, Released, Stand, Stop,
};
use crate::health::{Checks, MapProbe, Readiness, StateProbe, StorageProbe};
use crate::listen::{Binder, Socket};
use crate::members::Members;
use crate::proxy_auth::ProxyAuthLayer;
use crate::serve::{
    AbortOnDrop, ServeError, drain_and_stop, flow_control, keepalive, log_state, map_of,
    members_of, membership, serve_api, stop_api, sweeping, worker_peers,
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
    "releases_dir",
    "readyz_store_timeout_ms",
    "worker_tls_cert",
    "worker_tls_key",
    "worker_client_ca",
    "tls_reload_interval_secs",
    "worker_deny_list",
    "proxy_auth_file",
    "heartbeat_interval_ms",
    "progress_interval_secs",
    "http2_keepalive_interval_secs",
    "http2_keepalive_timeout_secs",
    "reapi_max_streams",
    "worker_max_streams",
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
    "history_objects",
    "history_index_root",
    "log_dir",
    "log_max_bytes",
    "log_files",
];

/// The ids of the three flags that give the frontend its client identity toward storage,
/// in the order [`FrontendFlags::tls`] reads them.
const TLS: [&str; 3] = ["storage_client_cert", "storage_client_key", "storage_ca"];

/// The ids of the flags of a handover: the control socket, how long a frontend waits to
/// stand down, and how often one that has handed over looks for its callers' results.
const HANDOVER: [&str; 3] = [
    "handover_socket",
    "handover_quiesce_secs",
    "handover_follow_interval_ms",
];

/// The id of the flag that turns on the object-store registration API and names where its
/// secret files are kept.
const SECRETS_DIR: &str = "object_store_secrets_dir";

/// The ids of the flags that say which instance this is, how long a holder waits for its
/// successor to serve, the name the successor's certificate carries, and Envoy's check
/// settings (interval, timeout, unhealthy threshold).
const INSTANCE: &str = "instance";
const WATCHDOG: &str = "handover_watchdog_secs";
const PEER_NAME: &str = "handover_peer_name";
const CHECK: [&str; 3] = [
    "envoy_check_interval_ms",
    "envoy_check_timeout_ms",
    "envoy_unhealthy_threshold",
];

/// The defaults of the timings: the quiesce wait and follow interval, the watchdog, and
/// Envoy's check (the installer's box render).
const QUIESCE_SECS: &str = "10";
const FOLLOW_INTERVAL_MS: &str = "200";
const WATCHDOG_SECS: &str = "15";
const CHECK_DEFAULTS: [&str; 3] = ["1000", "1000", "2"];

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
    /// How this frontend hands over to a successor and takes over from a predecessor;
    /// `None` without `--handover-socket`.
    pub handover: Option<Handover>,
    /// The admin port and whether to start as a candidate ([`crate::managed`]); `None`
    /// without `--admin-listen`.
    pub admin: Option<crate::managed::AdminFlags>,
    /// `--object-store-secrets-dir`: where the secret keys of the registered object-store
    /// connections are kept (one 0600 file each), which also turns on the
    /// `/v1/object-stores` routes of the operator API. `None`: they answer 404.
    pub object_store_secrets_dir: Option<PathBuf>,
}

/// The long names of the flags of [`Args`] that `kbf frontend` hides and refuses by name
/// ([`parse`]): every flag outside [`ALLOWED`] and the thread flags.
#[must_use]
pub fn refused_flags() -> std::collections::BTreeSet<String> {
    crate::flag_set::refused_names(ALLOWED)
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
    .arg(
        Arg::new(HANDOVER[0])
            .long("handover-socket")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires("state_store")
            .help(
                "The control socket of a handover (mode 0600, this user only). A frontend \
                 serving binds it; one started while another serves on the same \
                 --state-store (and the other --instance) binds its own listeners, asks the \
                 other to hand over its scheduler state and its daemons, and takes them \
                 without failing a build. The other goes not ready once Envoy has seen the \
                 new one, forwards the calls Envoy still sends it, and exits when it holds \
                 nothing.",
            ),
    )
    .arg(
        Arg::new(INSTANCE)
            .long("instance")
            .value_name("a|b")
            .value_parser(clap::value_parser!(Instance))
            .requires(HANDOVER[0])
            .help(
                "Which of the two instances this is. Instance a listens where --listen, \
                 --worker-listen and --api-listen say, instance b ten ports above each (port \
                 0 stays 0): the two blocks Envoy's configuration names. A handover goes \
                 to the other instance; without --instance a frontend refuses one.",
            ),
    )
    .arg(
        Arg::new(HANDOVER[1])
            .long("handover-quiesce-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64))
            .default_value(QUIESCE_SECS)
            .requires(HANDOVER[0])
            .help(
                "The longest a frontend that hands over waits for the Execute calls between \
                 their request and their queueing, and the results being taken, before it \
                 stands down anyway.",
            ),
    )
    .arg(
        Arg::new(HANDOVER[2])
            .long("handover-follow-interval-ms")
            .value_name("MILLISECONDS")
            .value_parser(clap::value_parser!(u64).range(1..))
            .default_value(FOLLOW_INTERVAL_MS)
            .requires(HANDOVER[0])
            .help(
                "How often a frontend that has handed over looks in the state store for \
                 the results of the Execute and WaitExecution streams it kept open.",
            ),
    )
    .arg(
        Arg::new(WATCHDOG)
            .long("handover-watchdog-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(1..))
            .default_value(WATCHDOG_SECS)
            .requires(HANDOVER[0])
            .help(
                "How long after it has handed over a frontend waits for its successor to \
                 serve. If the successor does not (or its control connection closes), the \
                 frontend goes not ready itself, so Envoy answers cleanly instead of choosing \
                 a frontend that can start nothing new.",
            ),
    )
    .arg(
        Arg::new(PEER_NAME)
            .long("handover-peer-name")
            .value_name("DNS_NAME")
            .requires(HANDOVER[0])
            .help(
                "The DNS name the REAPI certificate of the other instance carries. A frontend \
                 whose REAPI listener has a certificate forwards the calls it still receives \
                 after a handover over TLS to the successor, verified against --storage-ca \
                 and this name; without it such a frontend refuses a handover.",
            ),
    )
    .args(crate::managed::flags::args())
    .arg(
        Arg::new(SECRETS_DIR)
            .long("object-store-secrets-dir")
            .value_name("DIR")
            .value_parser(clap::value_parser!(PathBuf))
            .requires_all(["api_listen", "api_token_file", "state_store"])
            .help(
                "Turns on the object-store registration routes of the operator API \
                 (/v1/object-stores, docs/api.md) and names the directory where the secret \
                 key of each registered connection is kept, one file of mode 0600 each, in a \
                 directory made 0700. The directory must be readable by every frontend \
                 child that serves at once and left out of any backup of the host. The \
                 registration itself (endpoints, buckets, the audit log) is kept in the \
                 state store. Without it those routes answer 404.",
            ),
    )
    .arg(check_arg(0, "envoy-check-interval-ms", "MILLISECONDS", "The time between two of Envoy's health checks of a frontend (the cluster's `interval`)."))
    .arg(check_arg(1, "envoy-check-timeout-ms", "MILLISECONDS", "How long Envoy's health check waits for an answer (the cluster's `timeout`)."))
    .arg(check_arg(2, "envoy-unhealthy-threshold", "N", "How many of Envoy's checks in a row must fail before it stops choosing a frontend (`unhealthy_threshold`). The three give the settle: a frontend waits interval x threshold + timeout + interval before it goes not ready (Envoy has then seen the successor) and again before it exits (Envoy then sends it nothing)."))
}

/// One of the three flags of Envoy's check settings: a positive number, with the box
/// render's value as its default.
fn check_arg(n: usize, long: &'static str, value_name: &'static str, help: &'static str) -> Arg {
    Arg::new(CHECK[n])
        .long(long)
        .value_name(value_name)
        .value_parser(clap::value_parser!(u64).range(1..=3_600_000))
        .default_value(CHECK_DEFAULTS[n])
        .requires(HANDOVER[0])
        .help(help)
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
    let mut server = Args::from_arg_matches(m)?;
    let instance = m.get_one::<Instance>(INSTANCE).copied();
    if let Some(instance) = instance {
        place(&mut server, instance).map_err(refuse)?;
    }
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
    let handover = m.get_one::<PathBuf>(HANDOVER[0]).map(|socket| Handover {
        socket: socket.clone(),
        instance,
        quiesce: Duration::from_secs(m.get_one::<u64>(HANDOVER[1]).map_or(0, |secs| *secs)),
        follow_interval: Duration::from_millis(m.get_one::<u64>(HANDOVER[2]).map_or(0, |ms| *ms)),
        watchdog: Duration::from_secs(m.get_one::<u64>(WATCHDOG).map_or(0, |secs| *secs)),
        envoy: envoy_check(m),
        peer_name: m.get_one::<String>(PEER_NAME).cloned(),
    });
    let admin = crate::managed::flags::parse(m).map_err(refuse)?;
    Ok(FrontendFlags {
        server,
        storage,
        tls,
        handover,
        admin,
        object_store_secrets_dir: m.get_one::<PathBuf>(SECRETS_DIR).cloned(),
    })
}

/// Envoy's check settings the flags give (their defaults when `m` has none).
fn envoy_check(m: &ArgMatches) -> EnvoyCheck {
    let number = |n: usize| m.get_one::<u64>(CHECK[n]).copied().unwrap_or(0);
    EnvoyCheck {
        interval: Duration::from_millis(number(0)),
        timeout: Duration::from_millis(number(1)),
        unhealthy_threshold: u32::try_from(number(2)).unwrap_or(u32::MAX),
    }
}

/// Moves the listeners of `server` to the port block of `instance`: the flags' for a, ten
/// ports above for b ([`Instance::port`]).
///
/// # Errors
/// A port of instance b would run past 65535.
fn place(server: &mut Args, instance: Instance) -> Result<(), String> {
    let shift = |flag: &str, addr: SocketAddr| {
        instance
            .port(addr.port())
            .map(|port| SocketAddr::new(addr.ip(), port))
            .ok_or_else(|| {
                format!(
                    "--instance b listens {offset} ports above --{flag}, and {addr} leaves no \
                     room below 65535",
                    offset = Instance::SECOND_OFFSET
                )
            })
    };
    server.listen = shift("listen", server.listen)?;
    server.worker_listen = shift("worker-listen", server.worker_listen)?;
    server.api_listen = server
        .api_listen
        .map(|addr| shift("api-listen", addr))
        .transpose()?;
    Ok(())
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
    pub(crate) escalation: Option<Escalation>,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
    /// Tasks that run while the frontend serves and end with it: the one that expires
    /// the state store's rows, and the control socket of a handover.
    background: Vec<BoxFuture<'static, ()>>,
    /// Tells the service manager the frontend serves, once its probes pass.
    ready: Ready,
    /// What gives the roles up and takes them back ([`crate::managed`]); with a state
    /// store only.
    stand: Option<Stand>,
}

impl FrontendHandle {
    /// The frontend's drain ([`Drain`]): `POST /v1/frontend/drain` begins it, and the
    /// shutdown future given to [`start_frontend`] does.
    #[must_use]
    pub fn drain(&self) -> Arc<Drain> {
        Arc::clone(&self.drain)
    }

    /// What gives this frontend's roles up, if it has a state store.
    pub(crate) const fn stand(&self) -> Option<&Stand> {
        self.stand.as_ref()
    }

    /// The line the frontend prints once it is bound:
    /// `kbf-frontend <version> reapi=<addr> worker=<addr>[ api=<addr>] storage=<url>`.
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// This frontend, sending `READY=1` to `target` (the service manager's
    /// `$NOTIFY_SOCKET`; `None` tells no one) once it serves and its probes pass.
    #[must_use]
    pub fn notifying(mut self, target: Option<OsString>) -> Self {
        self.ready.notify = target;
        self
    }

    /// Serves every listener until the shutdown future given to [`start_frontend`]
    /// completes and the open Execute streams finish or `--shutdown-timeout-secs` runs
    /// out (the drain `kbf-server` runs: [`crate::serve::drain_and_stop`]).
    ///
    /// # Errors
    /// A listener fails while serving.
    pub async fn serve(self) -> Result<(), ServeError> {
        let tasks = self
            .background
            .into_iter()
            .chain([self.ready.run().boxed()]);
        let _tasks: Vec<AbortOnDrop> = tasks
            .map(|task| AbortOnDrop(tokio::spawn(task).abort_handle()))
            .collect();
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
/// This is a start with no predecessor: with `--handover-socket` it binds the control
/// socket, and a frontend already serving on it is not asked (see
/// [`start_or_take_over`]).
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
    bind_frontend(flags, boxed(shutdown))
}

fn boxed<S, F>(shutdown: S) -> Shutdown
where
    S: FnOnce() -> io::Result<F> + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    Box::new(move || shutdown().map(|f| Box::pin(f) as Pin<Box<dyn Future<Output = ()> + Send>>))
}

/// [`start_frontend`], not generic over the shutdown future: every test's start counts
/// toward the same function's coverage.
fn bind_frontend(
    flags: &FrontendFlags,
    shutdown: Shutdown,
) -> Result<FrontendHandle, Box<dyn Error>> {
    let prepared = Prepared::new(flags)?;
    let state = flags
        .server
        .state_store
        .as_ref()
        .map(crate::StateStoreUri::open)
        .transpose()?;
    prepared.start(state, shutdown, None)
}

/// A start over a state store that is already open and held (`state`), with no
/// predecessor: [`start_frontend`] with the store given instead of opened from
/// `--state-store`. `stop` completes to stop the frontend.
pub(crate) fn start_over(
    flags: &FrontendFlags,
    state: crate::StateStore,
    stop: &Stop,
) -> Result<FrontendHandle, Box<dyn Error>> {
    let stop = stop.clone();
    Prepared::new(flags)?.start(Some(state), Box::new(move || Ok(stop.fired())), None)
}

/// A start over a state store whose generation was just taken, on the REAPI and operator
/// API sockets a candidate bound ([`crate::managed`]); only the worker listener is bound
/// here.
pub(crate) fn start_over_sockets(
    flags: &FrontendFlags,
    sockets: crate::managed::Sockets,
    state: crate::StateStore,
    stop: Stop,
) -> Result<FrontendHandle, Box<dyn Error>> {
    let prepared = Prepared::over(flags, sockets)?;
    prepared.start(Some(state), Box::new(move || Ok(stop.fired())), None)
}

/// Starts the frontend the flags describe, taking over from the one serving on
/// `--handover-socket`, if there is one (see [`crate::handover`]): it binds its own
/// listeners, tells that frontend where it listens, waits for it to stand down, and opens
/// the state store at the generation it held. Without the flag, or with no frontend on the
/// socket, this is [`start_frontend`].
///
/// `shutdown` is called first. A stop before the serving frontend is told this one is
/// ready leaves it serving, untouched.
///
/// # Errors
/// As [`start_frontend`]; the frontend serving refuses the handover (the same instance,
/// another state file or a REAPI address that is not loopback), or fails to stand down;
/// the state store is not at the generation the serving frontend held; or `shutdown`
/// completes before the handover is agreed.
pub async fn start_or_take_over<S, F>(
    flags: &FrontendFlags,
    shutdown: S,
) -> Result<FrontendHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F> + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    take_over(flags, boxed(shutdown)).await
}

/// [`start_or_take_over`], not generic.
async fn take_over(
    flags: &FrontendFlags,
    shutdown: Shutdown,
) -> Result<FrontendHandle, Box<dyn Error>> {
    let mut stopping = shutdown()?;
    let Some(handover) = &flags.handover else {
        return bind_frontend(flags, Box::new(move || Ok(stopping)));
    };
    // Everything that does not need the state store is built, and the listeners are
    // bound, before the serving frontend is asked: a start that fails here has changed
    // nothing, and until it is told this one is ready a stop leaves it untouched.
    let prepared = Prepared::new(flags)?;
    let asked = async {
        let hello = prepared.hello()?;
        Ok::<_, Box<dyn Error>>(handover::knock(&handover.socket, &hello).await?)
    };
    let asked = tokio::select! {
        biased;
        () = &mut stopping => return Err("stopped before the handover was agreed".into()),
        asked = asked => asked?,
    };
    let Some(offer) = asked else {
        // Nobody serves: a start with no predecessor.
        let state = state_uri(flags)?.open()?;
        return prepared.start(Some(state), Box::new(move || Ok(stopping)), None);
    };
    let generation = offer.generation;
    let released = offer.ready().await?;
    let state = state_uri(flags)?.open_after_handover(generation)?;
    prepared.start(Some(state), Box::new(move || Ok(stopping)), Some(released))
}

/// The `--state-store` of a frontend that hands over (the flag requires it). A Postgres
/// store cannot be handed over yet: it has no way to give up its writer role
/// (`Store::demote`), and the takeover that replaces the handover comes with side-by-side
/// Postgres (`docs/design/frontend-proxy.md`, section 8), so the combination is refused
/// before the frontend serving is asked anything.
fn state_uri(flags: &FrontendFlags) -> Result<&crate::StateStoreUri, HandoverError> {
    match flags.server.state_store.as_ref() {
        None => Err(HandoverError::NoState),
        Some(crate::StateStoreUri::Postgres(_)) => Err(HandoverError::PostgresStore),
        Some(uri) => Ok(uri),
    }
}

/// A frontend's start up to the state store: everything the flags decide that needs
/// neither the store nor the handover, and the listening sockets, bound.
struct Prepared<'a> {
    flags: &'a FrontendFlags,
    listeners: crate::serve::Listeners,
    proxy: Option<crate::serve::Behind>,
    policy: kbf_auth::Policy,
    api: Option<crate::serve::Api>,
    channel: tonic::transport::Channel,
    sockets: Bound,
}

/// The three listening sockets.
pub(crate) struct Bound {
    reapi: Socket,
    worker: Socket,
    api: Option<Socket>,
}

impl Bound {
    /// Binds the listeners one after the other through `binder`, so that they share its
    /// one retry budget: made once per start, it keeps the whole bind phase below the
    /// unit's start timeout whatever number of addresses wait for a late interface.
    pub(crate) fn bind(
        reapi: SocketAddr,
        worker: SocketAddr,
        api: Option<SocketAddr>,
        binder: &Binder,
    ) -> Result<Self, ServeError> {
        Ok(Self {
            reapi: Socket::bind(reapi, binder)?,
            worker: Socket::bind(worker, binder)?,
            api: api.map(|api| Socket::bind(api, binder)).transpose()?,
        })
    }
}

impl<'a> Prepared<'a> {
    /// Reads the flags and binds the listeners.
    fn new(flags: &'a FrontendFlags) -> Result<Self, Box<dyn Error>> {
        Self::bound(flags, |listeners, api| {
            Bound::bind(
                listeners.reapi,
                listeners.worker,
                api.map(|api| api.listen),
                &Binder::new(),
            )
        })
    }

    /// [`Prepared::new`] on the REAPI and operator API sockets of a candidate: only the
    /// worker listener is bound.
    fn over(
        flags: &'a FrontendFlags,
        sockets: crate::managed::Sockets,
    ) -> Result<Self, Box<dyn Error>> {
        Self::bound(flags, |listeners, _| {
            Ok(Bound {
                reapi: sockets.reapi,
                worker: Socket::bind(listeners.worker, &Binder::new())?,
                api: sockets.api,
            })
        })
    }

    fn bound(
        flags: &'a FrontendFlags,
        sockets: impl FnOnce(
            &crate::serve::Listeners,
            Option<&crate::serve::Api>,
        ) -> Result<Bound, ServeError>,
    ) -> Result<Self, Box<dyn Error>> {
        let args = &flags.server;
        let listeners = args.listeners()?;
        let proxy = args.proxy()?;
        let policy = args.reapi_auth_policy()?;
        let api = args.api()?;
        let channel = flags.storage_endpoint()?.connect_lazy();
        let sockets = sockets(&listeners, api.as_ref())?;
        Ok(Self {
            flags,
            listeners,
            proxy,
            policy,
            api,
            channel,
            sockets,
        })
    }

    /// What this frontend tells the one serving, to be allowed to take over.
    fn hello(&self) -> Result<Message, HandoverError> {
        Ok(Message::Hello {
            version: handover::wire::PROTOCOL,
            instance: self.flags.handover.as_ref().and_then(|h| h.instance),
            reapi: self.sockets.reapi.local,
            state: state_uri(self.flags)?.identity(),
        })
    }

    /// Opens everything over `state` and serves on the listeners bound. `predecessor` is
    /// the frontend this one took over from, to be told when this one serves.
    fn start(
        self,
        state: Option<crate::StateStore>,
        shutdown: Shutdown,
        predecessor: Option<Released>,
    ) -> Result<FrontendHandle, Box<dyn Error>> {
        let Self {
            flags,
            listeners,
            proxy,
            policy,
            api,
            channel,
            sockets,
        } = self;
        let args = &flags.server;
        let state = crate::start::with_members(state, args.members());
        let state = crate::history::with_history(state, &args.history)?;
        let (state, history) = crate::history::take(state);
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
            state_store.clone(),
            timeout,
        ));
        let backend = match &routed {
            Some(routed) => Backend::routed(Arc::clone(routed), storage),
            None => Backend::remote(storage),
        };
        // A failed run's logs are read from the storage node, under a budget of this
        // frontend's own: it serves no blob route to share one with.
        let history = history.map(|history| {
            history.with_blobs(
                Arc::new(backend.clone()),
                crate::http_blobs::ReadBudget::default(),
            )
        });

        let (recorder, recording) = Recorder::new(listeners.invocation_memory, RECORDER_CHANNEL);
        let recording = crate::history::install(recording, history.as_ref());
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

        let (mut worker_server, peers, worker_tls) = worker_peers(
            keepalive(
                flow_control(Server::builder(), listeners.stream_limits.worker),
                listeners.streams,
            ),
            listeners.worker_tls,
            proxy.as_ref(),
        );
        let tls_sources: Vec<TlsSource> = listeners
            .reapi_tls
            .iter()
            .chain(&worker_tls)
            .cloned()
            .collect();
        let (closer, closing) = kbf_front::closing();
        let (drain, goaway) = Drain::new(closer, Arc::clone(&readiness));
        let drain = Arc::new(drain);
        let worker_stop = Stop::new();
        let Bound {
            reapi,
            worker,
            api: api_socket,
        } = sockets;
        let (reapi_addr, worker_addr) = (reapi.local, worker.local);
        let api_addr = api_socket.as_ref().map(|socket| socket.local);
        // Both listeners are closed the moment the drain begins and the worker listener
        // is stopped, whatever the transport is doing then.
        let (reapi_incoming, reapi_latch) = reapi.grpc(listeners.reapi_tls)?;
        drain.begins().closes(reapi_latch);
        let (worker_incoming, worker_latch) = worker.grpc(worker_tls)?;
        worker_stop.closes(worker_latch);
        let api_listener = api_socket.map(Socket::http).transpose()?;
        let (token, releases) = api.map_or((None, None), |api| (api.token, api.releases));
        let update_store = state_store.clone();
        let update_rows = state_store.clone().map(crate::update::Rows::new);
        let object_stores = flags
            .object_store_secrets_dir
            .as_deref()
            .map(|dir| open_object_stores(dir, state_store.clone()))
            .transpose()?;
        let offers = crate::release::offers::offers_of(
            releases.clone(),
            update_rows.clone(),
            listeners.tick,
        );
        let api_routes = crate::api::router(
            Arc::clone(&farm),
            token.clone(),
            Arc::clone(&drain),
            members.clone(),
            update_rows.clone(),
            // The enroll service and the CA that gives it a pin come with the frontend's
            // enroll listener; until it exists, the enrollment routes answer 404.
            None,
        )
        .merge(crate::verbs::routes(token.clone(), members.clone()))
        .merge(crate::release::router(releases.clone(), token.clone()))
        .merge(crate::object_store::api::routes(
            token.clone(),
            object_stores,
        ))
        .merge(crate::update_hosts::router(
            Arc::clone(&farm),
            update_rows.clone(),
            crate::update_hosts::FarmReaders::from_policy(&policy),
        ))
        .merge(crate::release::desired::router(
            releases.clone(),
            update_rows,
            token,
            Arc::clone(&farm),
        ))
        .merge(crate::health::router_of(Arc::clone(&checks)))
        .merge(metrics_route(
            backend.clone(),
            history.as_ref().map(crate::history::History::stats),
            members.clone(),
            tls_sources,
        ));
        // The storage health, if there are members.
        let api_routes = members.iter().fold(api_routes, |routes, members| {
            routes.merge(crate::storage_health::router(Arc::clone(members)))
        });

        let proxy_secret = proxy.as_ref().map(|behind| Arc::clone(&behind.secret));
        let mut reapi_server = keepalive(
            flow_control(Server::builder(), listeners.stream_limits.reapi),
            listeners.streams,
        )
        .layer(ProxyAuthLayer::new(proxy_secret.clone()));
        let membership = membership(members.clone(), &peers);
        let sessions = WorkerService::new(
            Arc::clone(&farm),
            peers,
            listeners.heartbeat_interval,
            listeners.hello_wait,
        );
        // Daemons are sent the members' map: Welcome.map and a MapUpdate at each bump.
        let sessions = map.into_iter().fold(sessions, WorkerService::with_map);
        let sessions = offers
            .into_iter()
            .fold(sessions, WorkerService::with_offers);
        let worker_service = WorkerServer::new(sessions)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);

        let (ticker, progress) = kbf_front::progress(listeners.streams.progress_interval);
        let authorizers = Arc::new(policy.authorizers);
        let bootstrap = Owner::new(&flags.storage.uri().to_string(), channel);
        let owners = match routed {
            Some(routed) => Owners::routed(routed, bootstrap),
            None => Owners::single(bootstrap),
        };
        let proxy = Proxy::new(owners, Arc::clone(&authorizers));
        let streams = kbf_front::OpenStreams { closing, progress };
        // With a handover configured, Execute and WaitExecution are forwarded to the
        // successor once this frontend has handed its roles over.
        let successor = Successor::unknown();
        let reapi_routes = match &flags.handover {
            Some(handover) => kbf_front::routes_relayed(
                &proxy,
                backend,
                Arc::clone(&farm),
                streams,
                authorizers,
                &recorder,
                default_memory,
                Relaying {
                    farm: Arc::clone(&farm) as Arc<dyn Handed>,
                    successor: successor.clone(),
                    follow_every: handover.follow_interval,
                },
            ),
            None => kbf_front::routes_proxied(
                &proxy,
                backend,
                Arc::clone(&farm),
                streams,
                authorizers,
                &recorder,
                default_memory,
            ),
        };
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

        // The tasks that run beside the listeners: the state store's expiry, which a
        // handover ends, and the control socket.
        let (sweeper, sweep_registration) = AbortHandle::new_pair();
        let mut background: Vec<BoxFuture<'static, ()>> = vec![
            Abortable::new(sweeping(members), sweep_registration)
                .map(drop)
                .boxed(),
        ];
        // Told by the control socket's task when a handed-over frontend holds nothing and
        // may exit, or could not stand down.
        let (departing, departed) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let stand = state_store.map(|store| Stand {
            drain: Arc::clone(&drain),
            farm: Arc::clone(&farm),
            worker: worker_stop.clone(),
            sweeper: sweeper.clone(),
            store,
            wait: flags
                .handover
                .as_ref()
                .map_or(crate::managed::QUIESCE_WAIT, |handover| handover.quiesce),
        });
        let holder = flags
            .handover
            .as_ref()
            .zip(stand.clone())
            .map(|(handover, stand)| {
                let facts = Facts {
                    instance: handover.instance,
                    state: args
                        .state_store
                        .as_ref()
                        .map_or_else(PathBuf::new, crate::StateStoreUri::identity),
                };
                let peer = Peer {
                    successor,
                    dial: dial_of(flags)?,
                };
                let leave = Leave {
                    watchdog: handover.watchdog,
                    settle: handover.envoy.settle(),
                    every: handover.follow_interval,
                };
                let uid = rustix::process::geteuid().as_raw();
                let holder = Holder::bind(&handover.socket, uid, facts, stand, peer, leave)?;
                Ok::<_, Box<dyn Error>>(holder.run(departing).boxed())
            })
            .transpose();
        background.extend(holder?);

        let shutdown = shutdown()?;
        let stopper = Arc::clone(&drain);
        let serving = async move {
            let history = crate::history::spawn(history);
            let _recording = AbortOnDrop(tokio::spawn(recording.run()).abort_handle());
            // The rolling-update controller; it starts no host until `auto_nodes` is set.
            let _updates =
                crate::update::spawn_controller(Arc::clone(&farm), update_store, listeners.tick)
                    .map(AbortOnDrop);
            let reapi_serve = reapi_server
                .add_routes(reapi_routes)
                .serve_with_incoming_shutdown(reapi_incoming, async {
                    let _ = goaway.await;
                });
            let worker_stopped = worker_stop.fired();
            let worker_serve = worker_server
                .add_service(worker_service)
                .add_service(crate::release::service(releases))
                .add_optional_service(membership)
                .serve_with_incoming_shutdown(worker_incoming, worker_stopped);
            // Told when the REAPI listener is done (see `serve::stop_api`).
            let (api_stop_when_done, api_stopped) = tokio::sync::oneshot::channel::<()>();
            let api_serve = serve_api(
                api_listener,
                api_routes,
                async {
                    let _ = api_stopped.await;
                },
                proxy_secret,
            )
            .fuse();
            tokio::pin!(api_serve);
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
            // A listener that ends without an error does not end the frontend: the worker
            // listener ends when a handover stops it, and the frontend goes on answering
            // the callers it kept until it holds nothing (`departed`).
            let outcome = tokio::select! {
                served = reapi_serve => {
                    reapi_done = true;
                    served.map_err(ServeError::from)
                }
                Err(e) = worker_serve => Err(ServeError::from(e)),
                Err(e) = &mut api_serve => Err(ServeError::Api(e)),
                () = ticking => Ok(()),
                () = ticker.run() => Ok(()),
                () = stopping => Ok(()),
                Ok(departure) = departed => departure.map_err(ServeError::Handover),
            };
            if reapi_done {
                stop_api(api_stop_when_done, api_serve).await;
            }
            crate::history::finish(history).await;
            outcome
        };
        let plain = args.plain_text_off_loopback();
        Ok(FrontendHandle {
            reapi: reapi_addr,
            worker: worker_addr,
            api: api_addr,
            drain,
            escalation: None,
            start_line: start_line(
                reapi_addr,
                worker_addr,
                api_addr,
                &flags.storage.uri().to_string(),
                &plain,
            ),
            serving: Box::pin(serving),
            background,
            ready: Ready {
                checks,
                notify: None,
                predecessor,
            },
            stand,
        })
    }
}

/// How a frontend that has handed over reaches its successor: the channel to a REAPI
/// listener at an address, plain text when this frontend's REAPI listener has no
/// certificate, and otherwise TLS verified against `--storage-ca` and
/// `--handover-peer-name`. A frontend without them cannot reach a TLS successor, and
/// refuses the handover (the reason is the `Err`) instead of forwarding calls nowhere.
///
/// # Errors
/// The CA file cannot be read.
fn dial_of(flags: &FrontendFlags) -> Result<crate::handover::Dial, ConfigError> {
    let tls = flags.server.reapi_tls_cert.is_some();
    let ca = match flags.storage_identity()? {
        Some((ca, _)) if tls => Some(ca),
        _ => None,
    };
    let name = flags.handover.as_ref().and_then(|h| h.peer_name.clone());
    Ok(Box::new(move |addr| {
        let channel = if tls {
            let (Some(ca), Some(name)) = (ca.clone(), name.clone()) else {
                return Err(
                    "this frontend's REAPI listener has a certificate, so the calls it forwards \
                     to its successor go over TLS: it needs --handover-peer-name (the name the \
                     successor's certificate carries) and the --storage-* flags (the CA)"
                        .into(),
                );
            };
            let config = ClientTlsConfig::new().ca_certificate(ca).domain_name(name);
            Endpoint::from_shared(format!("https://{addr}"))
                .and_then(|endpoint| endpoint.tls_config(config))
        } else {
            Endpoint::from_shared(format!("http://{addr}"))
        };
        // An address always makes a valid URI, and a CA tonic cannot use is only found when
        // the channel connects: the error here is the transport's own.
        channel
            .map(|endpoint| endpoint.connect_lazy())
            .map_err(Into::into)
    }))
}

/// The store `/readyz` probes: the open state store's, if there is one.
/// The object-store registration API over the state store, with secret files in `dir`
/// (`--object-store-secrets-dir`). No node reports yet, so every convergence is `false`.
fn open_object_stores(
    dir: &Path,
    state: Option<Arc<dyn kbf_state::Store>>,
) -> Result<Arc<crate::object_store::api::ObjectStoreApi>, Box<dyn Error>> {
    let store = state.ok_or("--object-store-secrets-dir needs --state-store")?;
    let runtime = tokio::runtime::Handle::try_current()?;
    crate::object_store::api::ObjectStoreApi::open(
        store,
        dir,
        runtime,
        Arc::new(crate::object_store::api::NoAcks),
    )
    .map(Arc::new)
    .map_err(|e| format!("--object-store-secrets-dir {}: {e}", dir.display()).into())
}

fn store_of(state: Option<&crate::StateStore>) -> Option<Arc<dyn kbf_state::Store>> {
    state.map(|state| Arc::clone(&state.store))
}

/// `GET /metrics`: what the service counts, in the Prometheus text format, history's
/// counters when it keeps history, the storage health of `members` when there are any, and
/// the reloads of the listeners' `tls` files.
fn metrics_route(
    backend: Backend,
    history: Option<Arc<crate::history::Stats>>,
    members: Option<Arc<Members>>,
    tls: Vec<TlsSource>,
) -> axum::Router {
    let tls = Arc::new(tls);
    let render = move || {
        let (backend, members, tls) = (backend.clone(), members.clone(), Arc::clone(&tls));
        let history = history.clone();
        async move {
            let mut text = backend.render_metrics();
            if let Some(history) = history {
                history.render(&mut text);
            }
            crate::metrics::scrape_health(&mut text, members.as_ref()).await;
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
        if flags.admin.is_some() {
            let managed =
                crate::managed::Managed::start(flags, crate::managed::StoreSource::Flags, first)
                    .await?;
            println!("{}", managed.start_line());
            managed
                .escalating(second)
                .notifying(std::env::var_os("NOTIFY_SOCKET"))
                .serve()
                .await?;
            return Ok(());
        }
        let mut frontend = start_or_take_over(flags, move || Ok(first)).await?;
        frontend.escalation = Some(second);
        println!("{}", frontend.start_line());
        // Set by the service manager of a `Type=notify` unit (not a flag: it names the
        // manager's socket, which only the manager knows).
        let frontend = frontend.notifying(std::env::var_os("NOTIFY_SOCKET"));
        Ok(frontend.serve().await?)
    })
}

#[cfg(test)]
mod tests;
