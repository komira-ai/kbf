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
//! **The state store** is required of the process, not of this module: `kbf frontend`
//! given no `--state-store` (and no `state-store` key in `kbf.toml`) exits 2 naming the
//! flag, `--check` included, before anything is bound (`kbf-cli`'s startup checks). The
//! entry points here still start without one, with the scheduler's state in memory and
//! the same warning as `kbf-server`'s, for the tests that need none. A store given is
//! opened and probed by `/readyz`; a `sqlite://` one starts and logs that SQLite is
//! removed in the next release ([`crate::StateStoreUri::open`]). A Postgres store that is not up
//! yet is waited for, up to `--state-store-wait-secs` (`state_wait` has the rules): a start
//! that uses the window up exits 1, every refusal exits 2.
//!
//! **The pack index** is off unless `--pack-index` is given (the flag's help has its rules).
//! The started frontend carries it ([`FrontendHandle::pack_index`]) and its worker listener
//! answers daemons' `kbf.worker.v1.PackIndex/Locate` from it ([`crate::pack_locate`]); without
//! the flag that service is not registered.
//!
//! **Packs** are off unless `--pack-store-endpoint` is given (`pack_flags` has the flags and
//! what they need): client uploads are then kept in packs ([`kbf_front::Packs`]); without it
//! no object store is built or contacted and the frontend serves exactly as it did. With the
//! pack index too, the frontend takes a daemon's `CommitPack` (it must read the pack to check it)
//! and says so in `Welcome.pack_commit`.
//!
//! **The executor** is off unless `--executor js-agent` is given (`executor_flags` has the flags):
//! the farm then runs the actions it places on hosts leased from the job scheduler's lease
//! server ([`crate::executor`]), and `FrontendHandle::serve` steps it beside the listeners and
//! releases its leases when the frontend stops. It needs the packs and the preparation of task
//! runs (below, with the `--job-*` listener its read grants point at): `--executor js-agent` is
//! refused when the flags are parsed without them, and where its settings disagree with them
//! (`executor_bind` lists the checks). By default, `--executor kbf-daemon`, nothing is built and
//! no lease server is contacted.
//!
//! **Task runs** are prepared only if `--task-secret-file` is given (`task_flags` has the flags
//! and what they need): the started frontend then carries a [`kbf_front::prepare_run::RunPrep`]
//! ([`FrontendHandle::run_prep`]) that gives a host executor, for each run, the key the task may
//! write, one presigned write of it to the tier store and a read grant for its inputs; without
//! the flag no secret file is read and the frontend serves exactly as it did.
//!
//! **The `cas/b/` janitor** is off unless `--task-janitor-key-file` is given (`janitor` has the
//! flags and the rule for an orphan): the leader then sweeps the task packs and large objects
//! under `cas/b/` that nothing recorded and are older than the grace, on an interval, through the
//! tier store's GC key, and counts what it did (`kbf_cas_b_janitor_*` on `GET /metrics`).
//!
//! **Readiness** (`GET /readyz`, `grpc.health.v1`) fails when the storage node does not
//! answer an action-cache lookup within `--readyz-store-timeout-ms`, when the state store (if
//! configured) does not answer a read, and when the storage map is not loaded (always
//! loaded today: [`crate::health::MapProbe`]). The storage check asks the `--storage`
//! node until the frontend routes by the map, and then the owners the map names for a
//! probe digest (it passes when one answers), so a farm that has lost its `--storage`
//! node is ready while its other members are. `READY=1` to the service manager does not
//! wait for this check (`ready::Ready`); `/readyz` stays failing until storage answers.
//!
//! **Several frontends share one state store.** A frontend hands its work to no successor:
//! it is stopped, or retired (`POST /v1/frontend/retire`, [`crate::retire`]), and the other
//! frontends of the store carry on. The flags of the removed handover and admin port
//! (`--handover-*`, `--instance`, `--envoy-*`, `--admin-listen`, `--await-takeover`) are
//! still accepted so a unit or `kbf.toml` written before the removal starts; they do
//! nothing, each one given logs a warning ([`RetiredHandoverFlags`]).
//!
//! **Draining.** A frontend over a state store is taken out of any load balancer by the
//! drain sequence of [`crate::sequence`], which `POST /v1/frontend/drain` and a stop signal
//! begin alike: `/readyz` answers 503 at once while everything is still served,
//! `--drain-lb-wait` later new Execute calls are refused and every listener is sent GOAWAY,
//! the open executions run until they are done or `--drain-deadline`, and the process exits
//! 0. Without a state store (tests) the drain is the plain one of [`crate::drain`].
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
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use clap::{Arg, ArgMatches, Command, FromArgMatches as _};
use futures::FutureExt as _;
use futures::future::{AbortHandle, Abortable, BoxFuture};
use kbf_auth::AuthenticateLayer;
use kbf_front::prepare_run::RunPrep;
use kbf_front::{
    Backend, MAX_MESSAGE_BYTES, Owner, Owners, Packs, Proxy, RECORDER_CHANNEL, Recorder,
    RequestMetadataLayer, Routed,
};
use kbf_pack_index::PackIndex;
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::worker::worker_server::WorkerServer;
use kbf_route::{DEFAULT_HOLD, Hold, TlsNodes};
use kbf_storage::Storage;
use kbf_storage::service::RemoteStorage;
use kbf_threads::Topology;
use kbf_tls::TlsSource;
use tonic::service::Routes;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server};

pub use self::executor_flags::{ExecutorFlags, JsAgentFlags};
pub use self::janitor::{Janitor, JanitorFlagError, JanitorFlags, JanitorMetrics};
pub use self::job_read::{JobReadError, JobReadFlags, JobTls, StoreGrantFlags};
pub use self::pack_flags::{PackFlagError, PackFlags};
pub use self::task_flags::{TaskFlagError, TaskFlags};
use crate::drain::Drain;
use crate::enroll::flags::EnrollFlags;
use crate::enroll::start as enroll_start;
use crate::executor::{Executor, Metrics as ExecutorMetrics, RunSource, Running};
use crate::farm::Farm;
use crate::front_maps::FrontMaps;
use crate::grpc_health::{HealthService, WATCH_INTERVAL};
use crate::health::{Checks, MapProbe, Readiness, StateProbe, StorageProbe};
use crate::listen::{Binder, Socket};
use crate::members::{Handoff, Members};
use crate::object_store::delivery::{Delivery, RegistrySource, Reports};
use crate::proxy_auth::ProxyAuthLayer;
use crate::ready::Ready;
use crate::retire::Retire;
use crate::retired::RetiredHandoverFlags;
use crate::sequence::{DrainTiming, Sequence, stopped};
use crate::serve::{
    AbortOnDrop, ServeError, cold_store, flow_control, keepalive, log_state, map_of, members_of,
    membership, serve_api_goaway, stop_api, sweeping, worker_peers,
};
use crate::stand::{QUIESCE_WAIT, Stand};
use crate::start::{Escalation, escalating, interruptions};
use crate::stop::Stop;
use crate::store_config::acks::Acks;
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
    "default_platform",
    "qos_allowed",
    "memory_ladder_gib",
    "no_memory_climb",
    "vm_host_floor_cpus",
    "vm_host_floor_mem_gib",
    "scheduler_core",
    "learned_sizes",
    "exclusive_actions",
    "placement_load",
    "placement_predictions",
    "whole_node_max",
    "whole_node_max_draining",
    "whole_node_keep_for_ordinary",
    "whole_node_allow_last_capable",
    "whole_node_drain_after_secs",
    "whole_node_drain_deadline_secs",
    "whole_node_reserve_backoff_secs",
    "whole_node_max_reserve_attempts",
    "finished_retention_secs",
    "shutdown_timeout_secs",
    "invocation_memory",
    "state_store",
    "pack_index",
    "member_ttl_secs",
    "member_dwell_secs",
    "history_objects",
    "history_index_root",
    "release_poll_every_secs",
    "release_poll_repo",
    "release_poll_refs",
    "log_dir",
    "log_max_bytes",
    "log_files",
];

/// The ids of the three flags that give the frontend its client identity toward storage,
/// in the order [`FrontendFlags::tls`] reads them.
const TLS: [&str; 3] = ["storage_client_cert", "storage_client_key", "storage_ca"];

/// The id of the flag that turns on the object-store registration API and names where its
/// secret files are kept.
const SECRETS_DIR: &str = "object_store_secrets_dir";

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
    /// The enroll listener (`--enroll-listen` and its companions); `None` without it.
    pub enroll: Option<EnrollFlags>,
    /// `--object-store-secrets-dir`: where the secret keys of the registered object-store
    /// connections are kept (one 0600 file each), which also turns on the
    /// `/v1/object-stores` routes of the operator API. `None`: they answer 404.
    pub object_store_secrets_dir: Option<PathBuf>,
    /// `--auto-promote` and its companions: a joiner is promoted by its own beat when it is
    /// filled ([`crate::members::Members::with_handoff`]); `None` without the flag, where only
    /// an operator promotes one.
    pub handoff: Option<Handoff>,
    /// `--drain-lb-wait` and `--drain-deadline`: the two waits of the drain sequence a
    /// frontend over a state store runs ([`crate::sequence`]).
    pub drain: DrainTiming,
    /// `--state-store-wait-secs`: how long the start waits for a state store that is not up
    /// yet before it gives up and exits 1. Zero does not wait.
    pub state_store_wait: Duration,
    /// The flags of the removed handover and admin port, which are accepted and ignored
    /// ([`RetiredHandoverFlags`]).
    pub retired: RetiredHandoverFlags,
    /// `--pack-store-endpoint` and its companions: the blobs clients upload are kept in packs
    /// in an object store. `None` without the endpoint, where nothing is packed.
    pub packs: Option<PackFlags>,
    /// `--executor` and its companions: where placed actions run.
    pub executor: ExecutorFlags,
    /// `--job-listen` and its companions: the listener a task on a worker reads its action's
    /// inputs from with a read grant. `None` without `--job-listen`, where there is none.
    pub job_read: Option<JobReadFlags>,
    /// `--task-secret-file` and its companions: the frontend prepares the runs of a host
    /// executor (the key, the presigned write and the read grant of each). `None` without the
    /// switch, where no run is prepared.
    pub task: Option<TaskFlags>,
    /// `--task-janitor-key-file` and its companions: the sweep of the task packs under `cas/b/`
    /// that nothing recorded. `None` without the key file, where nothing is swept.
    pub janitor: Option<JanitorFlags>,
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
    .args(crate::enroll::flags::args())
    .args(handoff::args())
    .args(drain_flags::args())
    .args(state_wait::args())
    .args(pack_flags::args())
    .args(job_read::args())
    .args(task_flags::args())
    .args(janitor::args())
    .args(executor_flags::args())
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
    .args(retired_args())
}

/// The arguments of [`RetiredHandoverFlags`], hidden from `--help`, to add to the command.
fn retired_args() -> Vec<Arg> {
    let retired = <RetiredHandoverFlags as clap::Args>::augment_args(Command::new("retired"));
    retired.get_arguments().cloned().collect()
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
    if let Err(why) = server.pack_index_url() {
        return Err(refuse(why.to_string()));
    }
    let enroll = crate::enroll::flags::from_matches(m).map_err(refuse)?;
    let mut executor = executor_flags::from_matches(m).map_err(refuse)?;
    let packs = pack_flags::from_matches(m);
    let job_read = job_read::from_matches(m);
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
    let task = task_flags::from_matches(m).map_err(refuse)?;
    let janitor = janitor::from_matches(m);
    janitor::check(janitor.as_ref(), packs.as_ref()).map_err(refuse)?;
    if let Some(js) = executor.js.as_mut() {
        let beside = executor_bind::Beside {
            task: task.as_ref(),
            job_read: job_read.as_ref(),
            packs: packs.as_ref(),
        };
        for warning in executor_bind::check(js, &beside).map_err(refuse)? {
            tracing::warn!("{warning}");
        }
    }
    Ok(FrontendFlags {
        server,
        storage,
        tls,
        enroll,
        object_store_secrets_dir: m.get_one::<PathBuf>(SECRETS_DIR).cloned(),
        handoff: handoff::from_matches(m),
        drain: drain_flags::from_matches(m),
        state_store_wait: state_wait::from_matches(m),
        retired: RetiredHandoverFlags::from_arg_matches(m)?,
        packs,
        executor,
        job_read,
        task,
        janitor,
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
/// place blobs on that this frontend will not look at. A call no member could answer is
/// held for [`DEFAULT_HOLD`] before it fails (there is no flag for the bound yet; the raw
/// ByteStream calls have the same bound, `proxy_owners`). Not generic, for the same reason
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
        Routed::over_tls(maps, Arc::new(TlsNodes::new(ca, identity)))
            .with_hold(DEFAULT_HOLD)
            .bootstrapped(storage.clone()),
    )))
}

/// The nodes the proxy calls: routed by the map when there is one, else the `--storage`
/// node `bootstrap`. The raw ByteStream calls that no node of their blob can answer are held
/// for [`DEFAULT_HOLD`], as `routed_of` holds the router's calls.
fn proxy_owners(routed: Option<Arc<Routed>>, bootstrap: Owner) -> Owners {
    let owners = match routed {
        Some(routed) => Owners::routed(routed, bootstrap),
        None => Owners::single(bootstrap),
    };
    owners.with_hold(Hold::new(DEFAULT_HOLD))
}

/// A started frontend: bound, not yet serving.
pub struct FrontendHandle {
    /// Where the REAPI listener is.
    pub reapi: SocketAddr,
    /// Where the worker listener is.
    pub worker: SocketAddr,
    /// Where the operator API listener is, if there is one.
    pub api: Option<SocketAddr>,
    /// Where the enroll listener is, if there is one.
    pub enroll: Option<SocketAddr>,
    /// Where the job-facing read listener is (`--job-listen`), if there is one.
    pub job_read: Option<SocketAddr>,
    enroll_tls: Option<kbf_tls::TlsSource>,
    drain: Arc<Drain>,
    /// Completes on the second stop signal; [`FrontendHandle::serve`] then closes the
    /// drain ([`Drain::close`]).
    pub(crate) escalation: Option<Escalation>,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
    /// Tasks that run while the frontend serves and end with it: the one that expires
    /// the state store's rows, and the renewal of the enroll listener's certificate.
    background: Vec<BoxFuture<'static, ()>>,
    /// Tells the service manager the frontend serves, once its probes pass.
    ready: Ready,
    /// The pack index `--pack-index` opened, if the flag was given.
    pack_index: Option<Arc<dyn PackIndex>>,
    /// The packs `--pack-store-endpoint` describes, if the flag was given.
    packs: Option<Packs>,
    /// The preparer of runs `--task-secret-file` describes, if the flag was given.
    run_prep: Option<Arc<RunPrep>>,
    /// The count of intake polls the farm has run to the end ([`Farm::intake_polls`]).
    intake_polls: tokio::sync::watch::Receiver<u64>,
    /// The count of sweeps the janitor of `cas/b/` has finished, if `--task-janitor-key-file`
    /// was given.
    janitor_sweeps: Option<tokio::sync::watch::Receiver<u64>>,
    /// The executor `--executor js-agent` builds, stepped while the frontend serves.
    executor: Option<Executor>,
}

impl FrontendHandle {
    /// The count of intake polls this frontend's farm has run to the end
    /// ([`Farm::intake_polls`]), told as each completes: a caller waits for polls by this
    /// event and not by time.
    #[must_use]
    pub fn intake_polls(&self) -> tokio::sync::watch::Receiver<u64> {
        self.intake_polls.clone()
    }

    /// The count of sweeps the janitor of `cas/b/` has finished (whole, stopped part way or by an
    /// error), told as each ends, or `None` without `--task-janitor-key-file`: a caller waits
    /// for sweeps by this event and not by time.
    #[must_use]
    pub fn janitor_sweeps(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.janitor_sweeps.clone()
    }

    /// The pack index `--pack-index` opened on the Postgres state store, or `None` without the
    /// flag. The worker listener's `PackIndex` service answers daemons from it, and with the
    /// pack store flags too, the frontend writes and reads it.
    #[must_use]
    pub fn pack_index(&self) -> Option<Arc<dyn PackIndex>> {
        self.pack_index.clone()
    }

    /// The preparer of the runs of a host executor `--task-secret-file` describes, or `None`
    /// without the flag: the executor asks it for the key, the presigned write and the read grant
    /// of each run ([`RunPrep::prepare_run`]).
    #[must_use]
    pub fn run_prep(&self) -> Option<Arc<RunPrep>> {
        self.run_prep.clone()
    }

    /// The frontend's drain ([`Drain`]): `POST /v1/frontend/drain` begins it, and the
    /// shutdown future given to [`start_frontend`] does.
    #[must_use]
    pub fn drain(&self) -> Arc<Drain> {
        Arc::clone(&self.drain)
    }

    /// The enroll listener's TLS configuration, if there is a listener: its
    /// `reloads().applied` counts the certificates the frontend's renewal task has put in
    /// place (what a test observes to see that the task runs).
    #[must_use]
    pub fn enroll_tls(&self) -> Option<kbf_tls::TlsSource> {
        self.enroll_tls.clone()
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
    /// out (the drain `kbf-server` runs: [`crate::serve::drain_and_stop`]); or, over a state
    /// store, until the drain sequence of [`crate::sequence`] is over, which the shutdown
    /// future and `POST /v1/frontend/drain` begin alike.
    ///
    /// # Errors
    /// A listener fails while serving; or the pack store does not refuse to replace an object
    /// (or cannot be written to), which is checked before anything is served.
    pub async fn serve(self) -> Result<(), ServeError> {
        if let Some(packs) = &self.packs {
            packs.check_create_only().await.map_err(ServeError::Packs)?;
        }
        let tasks = self
            .background
            .into_iter()
            .chain([self.ready.run().boxed()]);
        let _tasks: Vec<AbortOnDrop> = tasks
            .map(|task| AbortOnDrop(tokio::spawn(task).abort_handle()))
            .collect();
        // Started with the listeners: a frontend that never serves leases no host. Stopped
        // after the serving future (the listeners and their drain) is over, so the runs of
        // builds still open are not cut before the drain ends; the background tasks above are
        // not listeners and live to the end of this function.
        let mut running = self.executor.map(Running::spawn);
        let serving = escalating(self.serving, &self.drain, self.escalation);
        let served = match running.as_mut() {
            Some(running) => tokio::select! {
                served = serving => served,
                why = running.failed() => Err(ServeError::Executor(why)),
            },
            None => serving.await,
        };
        if let Some(running) = running {
            running.stop().await;
        }
        served
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
/// given), the pack index (`--pack-index`) and the packs (`--pack-store-endpoint`): for the
/// packs the credentials file is read, the cache directory made and the S3 client built, and
/// nothing is sent to the pack store until [`FrontendHandle::serve`] checks it. The storage
/// node is dialed when first used.
///
/// `shutdown` is called once, after the flags are checked and before anything is bound,
/// and returns the future whose completion stops the frontend; the binary's is
/// [`interrupted`].
///
/// # Errors
/// A flag the frontend refuses, a TLS file that cannot be read, a state store that
/// cannot be opened, pack flags that cannot be opened (a credentials file that is missing or
/// readable by others, an endpoint the store client does not take, a writer name a key cannot
/// hold, a cache directory that cannot be made), a listener that cannot be bound, or a
/// `shutdown` that fails.
pub fn start_frontend<S, F>(
    flags: &FrontendFlags,
    shutdown: S,
) -> Result<FrontendHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F> + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    start_frontend_with(flags, shutdown, None)
}

/// [`start_frontend`] with the preparation of runs given to `--executor js-agent` instead of the
/// frontend's own ([`RunSource`]): the key mint and the preparation of a run's write and read
/// grant, for a test that prepares them its own way. A frontend of `--executor kbf-daemon`
/// takes none and ignores it.
///
/// # Errors
/// As [`start_frontend`]; and `--executor js-agent` without a `source`, a lease server token
/// that cannot be used, a CA file that cannot be read, or settings that break a rule of the
/// executor.
pub fn start_frontend_with<S, F>(
    flags: &FrontendFlags,
    shutdown: S,
    source: Option<RunSource>,
) -> Result<FrontendHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F> + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    // No branch here: this is the one generic function, and `bind_frontend` is the code.
    bind_frontend(flags, boxed(shutdown), source)
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
    source: Option<RunSource>,
) -> Result<FrontendHandle, Box<dyn Error>> {
    let prepared = Prepared::new(flags)?;
    // The flag's own refusals come before the wait for the store, as they did before it.
    flags.server.pack_index_url()?;
    let state = flags
        .server
        .state_store
        .as_ref()
        .map(|uri| state_wait::open(uri, flags.state_store_wait))
        .transpose()?;
    let pack_index = flags.server.open_pack_index()?;
    prepared.start(state, pack_index, shutdown, source)
}

/// A frontend's start up to the state store: everything the flags decide that needs no
/// store, and the listening sockets, bound.
struct Prepared<'a> {
    flags: &'a FrontendFlags,
    listeners: crate::serve::Listeners,
    proxy: Option<crate::serve::Behind>,
    policy: kbf_auth::Policy,
    api: Option<crate::serve::Api>,
    channel: tonic::transport::Channel,
    sockets: Bound,
    /// The enroll listener bound and its CA open, if `--enroll-listen` is given.
    enroll: Option<enroll_start::Prepared>,
    /// The job-facing read listener bound, its certificate and grant secret read, if
    /// `--job-listen` is given.
    job_read: Option<job_read::Prepared>,
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
        let args = &flags.server;
        args.check_scheduler_core()?;
        args.retired_release.warn();
        flags.retired.warn();
        let listeners = args.listeners()?;
        let proxy = args.proxy()?;
        let policy = args.reapi_auth_policy()?;
        let api = args.api()?;
        let channel = flags.storage_endpoint()?.connect_lazy();
        let binder = Binder::new();
        let enroll = flags
            .enroll
            .as_ref()
            .map(|enroll| enroll_start::prepare_with(enroll, &binder))
            .transpose()?;
        let reload = Duration::from_secs(args.tls_reload_interval_secs);
        let job_read = flags
            .job_read
            .as_ref()
            .map(|job| job_read::Prepared::new(job, reload, &binder))
            .transpose()?;
        let sockets = Bound::bind(
            listeners.reapi,
            listeners.worker,
            api.as_ref().map(|api| api.listen),
            &binder,
        )?;
        Ok(Self {
            flags,
            listeners,
            proxy,
            policy,
            api,
            channel,
            sockets,
            enroll,
            job_read,
        })
    }

    /// Opens everything over `state` and serves on the listeners bound. The `pack_index`
    /// (`--pack-index`) is opened already; the handle carries it.
    fn start(
        self,
        state: Option<crate::StateStore>,
        pack_index: Option<Arc<dyn PackIndex>>,
        shutdown: Shutdown,
        source: Option<RunSource>,
    ) -> Result<FrontendHandle, Box<dyn Error>> {
        let Self {
            flags,
            listeners,
            proxy,
            policy,
            api,
            channel,
            sockets,
            enroll,
            job_read,
        } = self;
        let args = &flags.server;
        let state = crate::start::with_members(state, args.members());
        let state = crate::history::with_history(state, &args.history)?;
        let (state, history) = crate::history::take(state);
        let members = members_of(state.as_ref(), flags.handoff);
        let map = map_of(None, members.as_ref())?;
        let readiness = Arc::new(Readiness::default());
        let timeout = listeners.store_probe_timeout;
        let storage = Arc::new(RemoteStorage::new(channel.clone(), 0));
        let state_store = store_of(state.as_ref());
        let routed = routed_of(members.as_ref(), flags, &storage)?;
        let packs = pack_flags::open(flags.packs.as_ref(), pack_index.as_ref())?;
        let janitor = janitor::open(
            flags.janitor.as_ref(),
            flags.packs.as_ref(),
            packs.as_ref(),
            pack_index.as_ref(),
            &readiness,
        )?;
        let janitor_metrics = janitor.as_ref().map(Janitor::metrics);
        let janitor_sweeps = janitor_metrics.as_ref().map(|metrics| metrics.finished());
        let checks = Arc::new(checks(
            Arc::clone(&readiness),
            storage.clone(),
            routed.clone(),
            state_store.clone(),
            timeout,
        ));
        let backend = match &routed {
            Some(routed) => Backend::routed_over(Arc::clone(routed), storage, packs.clone()),
            None => Backend::remote_over(storage, packs.clone()),
        };
        let run_prep = task_flags::open(flags.task.as_ref(), &backend)?;
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
        let defaults = kbf_front::Defaults {
            memory: listeners.memory_ladder.first(),
            platform: listeners.default_platform,
            qos: listeners.qos_allowed,
            exclusive_actions: listeners.exclusive_actions,
        };
        let durable = state.is_some();
        let farm = Farm::from_listeners(backend.clone(), &listeners).recording(recorder.clone());
        crate::serve::log_core(&farm);
        crate::serve::log_exclusive(listeners.exclusive_actions);
        let farm = Arc::new(farm.with_state(state)?);
        log_state(durable, farm.term(), farm.lease_epoch());
        let source = source.or_else(|| run_prep.as_ref().map(RunSource::of));
        let executor = executor_of(flags, &farm, packs.as_ref(), &backend, source)?;

        let (mut worker_server, peers, worker_tls) = worker_peers(
            keepalive(
                flow_control(Server::builder(), listeners.stream_limits.worker),
                listeners.streams,
            ),
            listeners.worker_tls,
            proxy.as_ref(),
        );
        // The rows a revocation writes, beside the file: a revoked daemon is refused here.
        let peers = peers.with_denied_rows(members.clone());
        let mut tls_sources: Vec<TlsSource> = listeners
            .reapi_tls
            .iter()
            .chain(&worker_tls)
            .cloned()
            .collect();
        let (closer, closing) = kbf_front::closing();
        let (drain, goaway) = Drain::new(closer, Arc::clone(&readiness));
        let drain = Arc::new(drain);
        let worker_stop = Stop::new();
        // Sends the operator API's connections GOAWAY when the drain sequence refuses new
        // work; the listener goes on accepting ([`serve_api_goaway`]).
        let api_stop = Stop::new();
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
        let api_listeners = api_socket.map(Socket::http_pair).transpose()?;
        let token = api.and_then(|api| api.token);
        // The enroll listener (which is closed the moment the drain begins, like the
        // others) and the operator API's enrollment routes, which need the CA's pin: both
        // are off without `--enroll-listen`, and the routes then answer 404.
        let enroll_addr = enroll.as_ref().map(enroll_start::Prepared::local);
        let enrolled = enroll
            .map(|prepared| {
                let store = state_store
                    .clone()
                    .ok_or("--enroll-listen needs --state-store: the registry lives in it")?;
                prepared.serve(store, members.clone())
            })
            .transpose()?;
        let (enroll_api, enroll_serve, enroll_renewal, enroll_tls) = match enrolled {
            Some(enroll_start::Serving {
                api,
                listener,
                latch,
                renewal,
                tls,
            }) => {
                drain.begins().closes(latch);
                (Some(api), listener, Some(renewal), Some(tls))
            }
            None => (None, futures::future::pending().boxed(), None, None),
        };
        // The job-facing read listener, which reads the blobs the REAPI listener's reads do
        // (through the same backend) and is closed, like the others, the moment the drain
        // begins. Off without `--job-listen`.
        let job_addr = job_read.as_ref().map(job_read::Prepared::local);
        let job_read = job_read
            .map(|prepared| {
                prepared
                    .with_backfill(packs.clone())
                    .start(Arc::new(backend.clone()), job_read::Timeouts::default())
            })
            .transpose()?;
        let (job_stats, job_task) = match job_read {
            Some(job_read::Serving {
                latch,
                task,
                tls,
                read,
            }) => {
                drain.begins().closes(latch);
                tls_sources.extend(tls);
                (Some(read), Some(task))
            }
            None => (None, None),
        };
        let object_stores = flags
            .object_store_secrets_dir
            .as_deref()
            .map(|dir| open_object_stores(dir, state_store.clone().zip(members.clone())))
            .transpose()?;
        let delivery = object_stores.as_ref().map(|stores| stores.delivery.clone());
        // Set below, where the stand is built, if there is a state store to retire over.
        let retire = Arc::new(OnceLock::new());
        let api_routes = crate::api::router(
            Arc::clone(&farm),
            token.clone(),
            Arc::clone(&drain),
            Arc::clone(&retire),
            members.clone(),
            state_store.clone(),
            enroll_api,
        )
        .merge(crate::verbs::routes(token.clone(), members.clone()))
        .merge(crate::object_store::api::routes(
            token.clone(),
            object_stores.map(|stores| stores.api),
        ))
        .merge(crate::health::routes(&checks, &farm, members.as_ref()))
        .merge(metrics_route(
            backend.clone(),
            history.as_ref().map(crate::history::History::stats),
            members.clone(),
            tls_sources,
            Counters {
                packs: packs.clone(),
                job_read: job_stats,
                executor: executor.as_ref().map(|built| Arc::clone(&built.metrics)),
                janitor: janitor_metrics,
            },
            Arc::clone(&farm),
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
        let membership = membership(members.clone(), &peers, delivery.as_ref());
        // The four calls a host's updater makes about its own node.
        let node_calls = crate::node_self::routes(
            Arc::clone(&farm),
            members.clone(),
            state_store.clone(),
            peers.clone(),
        );
        let cold_store = cold_store(members.clone(), &peers, delivery.as_ref());
        // Only with `--pack-index`: otherwise a daemon's Locate is UNIMPLEMENTED; and its
        // CommitPack too is UNIMPLEMENTED without the pack store, which checks the pack.
        let commits = pack_flags::commits(packs.as_ref(), flags.packs.as_ref());
        let committing = crate::pack_locate::commit_served(pack_index.as_ref(), commits.as_ref());
        let locate = crate::pack_locate::service(pack_index.as_ref(), &peers, commits);
        let sessions = WorkerService::new(
            Arc::clone(&farm),
            peers,
            listeners.heartbeat_interval,
            listeners.hello_wait,
        )
        .with_host_floor(listeners.vm_host_floor)
        .with_pack_commit(committing)
        .with_placement_load(listeners.placement_load);
        // Daemons are sent the members' map: Welcome.map and a MapUpdate at each bump.
        let sessions = map.into_iter().fold(sessions, WorkerService::with_map);
        let worker_service = WorkerServer::new(sessions)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);

        let (ticker, progress) = kbf_front::progress(listeners.streams.progress_interval);
        let authorizers = Arc::new(policy.authorizers);
        let bootstrap = Owner::new(&flags.storage.uri().to_string(), channel);
        let proxy = Proxy::new(proxy_owners(routed, bootstrap), Arc::clone(&authorizers));
        let proxy = proxy.with_packs(packs.clone());
        let streams = kbf_front::OpenStreams { closing, progress };
        let reapi_routes = kbf_front::routes_proxied(
            &proxy,
            backend,
            Arc::clone(&farm),
            streams,
            authorizers,
            &recorder,
            defaults,
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

        // The tasks that run beside the listeners: the state store's expiry, which a retire
        // ends, and the renewal of the enroll listener's certificate.
        let (sweeper, sweep_registration) = AbortHandle::new_pair();
        let mut background: Vec<BoxFuture<'static, ()>> = vec![
            Abortable::new(sweeping(members), sweep_registration)
                .map(drop)
                .boxed(),
        ];
        let stand = state_store.is_some().then(|| Stand {
            drain: Arc::clone(&drain),
            farm: Arc::clone(&farm),
            worker: worker_stop.clone(),
            sweeper,
            wait: QUIESCE_WAIT,
        });
        // Set once, here: nothing has called the retire or the drain route before the
        // listeners serve. A frontend over a state store drains by the sequence of
        // [`crate::sequence`], and its stop signal begins it.
        if let Some(stand) = stand {
            let sequence = Sequence::new(stand.clone(), api_stop.clone(), flags.drain);
            let _ = retire.set(Retire::new(stand, sequence));
        }
        background.extend(enroll_renewal);
        background.extend(job_task);
        background.extend(janitor.map(|janitor| janitor.run().boxed()));

        let intake_polls = farm.intake_polls();
        let shutdown = shutdown()?;
        let stopper = Arc::clone(&drain);
        let serving = async move {
            let history = crate::history::spawn(history);
            let _recording = AbortOnDrop(tokio::spawn(recording.run()).abort_handle());
            let reapi_listening = reapi_server
                .add_routes(reapi_routes)
                .serve_with_incoming_shutdown(reapi_incoming, async {
                    let _ = goaway.await;
                });
            // A drain sequence ends with its own last step (the worker streams end
            // UNAVAILABLE), not when the REAPI listener has nothing left to answer.
            let reapi_serve = async {
                let served = reapi_listening.await;
                if stopper.is_sequenced() {
                    stopper.sequence_finished().await;
                }
                served
            };
            let worker_stopped = worker_stop.fired();
            let worker_serve = worker_server
                .add_routes(node_calls)
                .add_service(worker_service)
                .add_optional_service(membership)
                .add_optional_service(cold_store)
                .add_optional_service(locate)
                .serve_with_incoming_shutdown(worker_incoming, worker_stopped);
            // Told when the REAPI listener is done (see `serve::stop_api`).
            let (api_stop_when_done, api_stopped) = tokio::sync::oneshot::channel::<()>();
            let api_serve = serve_api_goaway(
                api_listeners,
                api_routes,
                api_stop.clone(),
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
            // What the other frontends of a shared state store did (`Farm::intake`).
            let intaking = farm.poll_intake();
            // The stop signal: the drain sequence of a frontend over a state store, else the
            // plain drain.
            let stopping = async {
                shutdown.await;
                stopped(&stopper, &retire, listeners.shutdown_timeout).await;
            };
            // The drain sequence begun by `POST /v1/frontend/drain` ends the frontend too;
            // never, when none runs.
            let drained = stopper.sequence_finished();
            let mut reapi_done = false;
            // A listener that ends without an error does not end the frontend: the worker
            // listener ends when a retire stops it, and the frontend goes on answering the
            // callers it kept.
            let mut enroll_serve = enroll_serve;
            let outcome = tokio::select! {
                served = reapi_serve => {
                    reapi_done = true;
                    served.map_err(ServeError::from)
                }
                Err(e) = worker_serve => Err(ServeError::from(e)),
                Err(e) = &mut api_serve => Err(ServeError::Api(e)),
                Err(e) = &mut enroll_serve => Err(ServeError::from(e)),
                () = ticking => Ok(()),
                () = intaking => Ok(()),
                () = ticker.run() => Ok(()),
                () = stopping => Ok(()),
                () = drained => Ok(()),
            };
            if reapi_done {
                // The operator API ends with the REAPI listener: its connections are sent
                // GOAWAY, finish what they answer, and it stops (`serve::stop_api`).
                api_stop.fire();
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
            enroll: enroll_addr,
            job_read: job_addr,
            enroll_tls,
            drain,
            escalation: None,
            start_line: start_line(
                reapi_addr,
                worker_addr,
                api_addr,
                enroll_addr,
                &flags.storage.uri().to_string(),
                &plain,
            ),
            serving: Box::pin(serving),
            background,
            ready: Ready {
                checks,
                notify: None,
            },
            pack_index,
            packs,
            run_prep,
            intake_polls,
            janitor_sweeps,
            executor: executor.map(|built| built.executor),
        })
    }
}

/// What `--object-store-secrets-dir` turns on: the operator routes, and what the worker
/// listener serves from the same registry.
struct ObjectStores {
    api: Arc<crate::object_store::api::ObjectStoreApi>,
    delivery: Delivery,
}

/// The object-store registration over the state store, with secret files in `dir`
/// (`--object-store-secrets-dir`): the operator API; the source `Membership` announces the
/// generation from and `ColdStore` answers from; and the acks the nodes' heartbeats leave,
/// which the API's convergence reads.
fn open_object_stores(
    dir: &Path,
    state: Option<(Arc<dyn kbf_state::Store>, Arc<Members>)>,
) -> Result<ObjectStores, Box<dyn Error>> {
    let (store, members) = state.ok_or("--object-store-secrets-dir needs --state-store")?;
    let runtime = tokio::runtime::Handle::try_current()?;
    let registry = crate::object_store::Registry::open(
        Arc::clone(&store),
        dir,
        Arc::new(crate::enroll::OsRandom),
    )
    .map(Arc::new)
    .map_err(|e| format!("--object-store-secrets-dir {}: {e}", dir.display()))?;
    let acks = Arc::new(Acks::new(store));
    let reports = Reports::new(members, Arc::clone(&acks), Arc::clone(&registry));
    let delivery = Delivery {
        source: Arc::new(RegistrySource::new(Arc::clone(&registry))),
        acks,
    };
    let probe = Arc::new(crate::object_store::probe::S3Probe::new(runtime));
    let api = crate::object_store::api::ObjectStoreApi::new(registry, probe, Arc::new(reports));
    Ok(ObjectStores {
        api: Arc::new(api),
        delivery,
    })
}

/// A built executor, and the counts it keeps.
struct ExecutorParts {
    executor: Executor,
    metrics: Arc<ExecutorMetrics>,
}

/// The executor `--executor js-agent` describes, over `farm`, with the metrics it counts into;
/// none for `kbf-daemon`, which builds nothing and contacts no lease server. The seams are the
/// frontend's own: the ingest of the packs, the CAS `backend` over the same packs, and the
/// `source` of the keys, writes and grants.
///
/// # Errors
/// `--executor js-agent` without the packs or without a `source` (the frontend makes one from
/// the run preparation `--task-secret-file` opens, which the parse of the flags requires), or
/// as [`JsAgentFlags::build`].
fn executor_of(
    flags: &FrontendFlags,
    farm: &Arc<Farm>,
    packs: Option<&Packs>,
    backend: &Backend,
    source: Option<RunSource>,
) -> Result<Option<ExecutorParts>, Box<dyn Error>> {
    let Some(js) = &flags.executor.js else {
        return Ok(None);
    };
    let packs = packs.ok_or("--executor js-agent needs --pack-store-endpoint")?;
    let source = source.ok_or(
        "--executor js-agent: this frontend was started without the preparation of task runs \
         (the key mint, the presigned write and the read grant): give --task-secret-file",
    )?;
    let front = js.front(packs.clone(), backend.clone(), source);
    let metrics = Arc::new(ExecutorMetrics::default());
    let executor = js.build(Arc::clone(farm), Arc::new(front), Arc::clone(&metrics))?;
    Ok(Some(ExecutorParts { executor, metrics }))
}

/// The store `/readyz` probes: the open state store's, if there is one.
fn store_of(state: Option<&crate::StateStore>) -> Option<Arc<dyn kbf_state::Store>> {
    state.map(|state| Arc::clone(&state.store))
}

/// What `GET /metrics` counts beside the always-present families, each only when its part of
/// the frontend is on.
struct Counters {
    /// The packs.
    packs: Option<Packs>,
    /// The job-facing read.
    job_read: Option<Arc<kbf_front::job_read::JobRead>>,
    /// The host executor.
    executor: Option<Arc<ExecutorMetrics>>,
    /// The janitor of `cas/b/`.
    janitor: Option<Arc<JanitorMetrics>>,
}

/// `GET /metrics`: what the service counts, in the Prometheus text format, history's
/// counters when it keeps history, the storage health of `members` when there are any, the
/// reloads of the listeners' `tls` files, the counters of the `packs` when there are any, those
/// of the job-facing read when there is one, and the pack reads the daemons of `farm` report
/// and its failed actions by class.
fn metrics_route(
    backend: Backend,
    history: Option<Arc<crate::history::Stats>>,
    members: Option<Arc<Members>>,
    tls: Vec<TlsSource>,
    counters: Counters,
    farm: Arc<Farm>,
) -> axum::Router {
    let Counters {
        packs,
        job_read,
        executor,
        janitor,
    } = counters;
    let tls = Arc::new(tls);
    let render = move || {
        let (backend, members, tls) = (backend.clone(), members.clone(), Arc::clone(&tls));
        let (history, packs, farm) = (history.clone(), packs.clone(), Arc::clone(&farm));
        let executor = executor.clone();
        let job_read = job_read.clone();
        let janitor = janitor.clone();
        async move {
            let mut text = backend.render_metrics();
            if let Some(history) = history {
                history.render(&mut text);
            }
            Packs::render_metrics_of(packs.as_ref(), &mut text);
            kbf_front::job_read::JobRead::render_metrics_of(job_read.as_deref(), &mut text);
            crate::metrics::render_farm(&mut text, &farm);
            if let Some(executor) = executor {
                executor.render(&mut text);
            }
            if let Some(janitor) = janitor {
                janitor.render(&mut text);
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
    enroll: Option<SocketAddr>,
    storage: &str,
    plain: &[&str],
) -> String {
    let api = api.map(|a| format!(" api={a}")).unwrap_or_default();
    let enroll = enroll.map(|a| format!(" enroll={a}")).unwrap_or_default();
    let warning = if plain.is_empty() {
        String::new()
    } else {
        format!(" warning=plaintext-off-loopback:{}", plain.join(","))
    };
    format!(
        "kbf-frontend {SERVER_VERSION} reapi={reapi} worker={worker}{api}{enroll} storage={storage}{warning}"
    )
}

/// `kbf frontend`: the logs, a runtime the thread flags size, the start line on stdout,
/// and serving until SIGINT or SIGTERM. Exit 2 for a start the frontend refuses, 1 for a state store that stayed unreachable
/// for `--state-store-wait-secs`.
#[must_use]
pub fn run(flags: &FrontendFlags) -> ExitCode {
    crate::log_file::init(&flags.server);
    match serve(flags) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf frontend: {e}");
            exit_of(e.as_ref())
        }
    }
}

/// The exit code of a start that failed: 1 for a state store still not reachable when
/// `--state-store-wait-secs` ran out (a start that was not refused, so a unit that
/// prevents a restart on exit 2 retries it), 2 for every refusal.
fn exit_of(e: &(dyn Error + 'static)) -> ExitCode {
    if e.is::<state_wait::NotReached>() {
        ExitCode::from(1)
    } else {
        ExitCode::from(2)
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
        // Set by the service manager of a `Type=notify` unit (not a flag: it names the
        // manager's socket, which only the manager knows).
        let frontend = frontend.notifying(std::env::var_os("NOTIFY_SOCKET"));
        Ok(frontend.serve().await?)
    })
}

mod drain_flags;
mod executor_bind;
mod executor_flags;
mod handoff;
mod janitor;
mod job_read;
mod pack_flags;
mod state_wait;
mod task_flags;

#[cfg(test)]
mod executor_tests;
#[cfg(test)]
mod pack_tests;
#[cfg(test)]
mod state_wait_tests;
#[cfg(test)]
mod tests;
