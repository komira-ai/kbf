//! `kbf storage`: the storage node (`docs/design/architecture-v2.md`, sections 3 and 13
//! P2). The cache the server has, the same stores and the same write tier, served over
//! gRPC by [`kbf_storage::service`] and nothing else: no REAPI, no scheduler, no
//! worker streams, no operator API (only `GET /metrics`, on `--metrics-listen` when
//! given), and no state store (the node reaches that only through a frontend, section 6).
//!
//! **The flags** are `kbf-server`'s own for what a storage node keeps (the object
//! stores, the metadata log, the write tier and its bounds, the logs, the thread pools,
//! the segment uploads: [`ALLOWED`] and the thread flags), and its own for where it
//! listens: `--storage-listen`, and `--storage-tls-cert`, `--storage-tls-key` and
//! `--storage-client-ca`, which together turn on mutual TLS and with it the check of the
//! role in each caller's certificate. Without them the node serves plain text, which
//! admits every call, so it is only for tests and trials on one machine: a plain-text
//! listener off loopback (a wildcard address included) is a start the node refuses.
//!
//! Every other flag of `kbf-server` is refused by name ([`parse`]), computed from
//! [`Args`]: a flag added there that is not listed in [`ALLOWED`] is refused here with
//! no edit.
//!
//! **The map** (section 6). Without `--frontend` it is one member: this node, for one box.
//! With `--frontend <url>` the node binds its listener, registers with that frontend
//! (retrying until it answers), and only then opens its cache, then serves and heartbeats
//! at the interval the frontend names ([`crate::join`]); over `https` it presents its own
//! certificate and trusts `--storage-client-ca`. It advertises `--advertise-addr`, or the
//! address it bound (which must then not be a wildcard). The node never talks to the state
//! store. The maps the frontend sends it (at registration, and at any heartbeat that finds
//! it behind) are kept in a [`kbf_mapsync::MapHolder`] ([`StorageHandle::maps`]), which the
//! node's storage service reads; the registration brings the first one, so it is held
//! before the node serves. Until it holds one the node serves nothing (UNAVAILABLE), it
//! answers a caller whose map is not the current one STALE_MAP, and under mutual TLS it is
//! a link of the chain that copies a write to the blob's second owner, with the repair
//! worker that makes a copy owed ([`StorageHandle::repairs`]); all as
//! [`crate::replication`] says.
//!
//! **Object names.** A registered node writes as its registration's member id and boot
//! counter (`kbf_meta::NodeWriter`): it commits `SetWriter` to its metadata log as the log
//! is (nothing raises the log's machine version), and its objects are
//! `<root>cas/<member>/<boot>/<seq>`, so a node whose metadata directory was wiped never
//! reuses a key (the state store hands each boot out once). A node without `--frontend`
//! names its objects in a writer epoch of its log, `<root><log id>/cas/<epoch>/<seq>`, as
//! a server does. Either reads both kinds.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Arg, ArgMatches, Command, FromArgMatches as _};
use kbf_front::store_metrics::StoreMetrics;
use kbf_front::{
    Cache, CacheStorage, Hold, MetaLog, MetaLogError, ObjectKeys, RebuildConfig, SegmentsConfig,
    VerifiedBlob,
};
use kbf_log::FarmLogId;
use kbf_mapsync::MapHolder;
use kbf_meta::{NodeWriter, Retention};
use kbf_objstore::{KeyPrefix, ObjectStore, StillHeld};
use kbf_storage::legacy::LegacyReads;
use kbf_storage::repair::{RepairConfig, Repairs};
use kbf_storage::service::CatchUp;
use kbf_store::StdFs;
use kbf_threads::Topology;
use kbf_threads::disk::Statvfs;
use kbf_tls::{TlsFiles, TlsSource};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

use crate::cold_apply::{ColdPick, Fixed, RegisteredOnly, Registrable};
use crate::cold_counted::CountedPick;
use crate::join::{self, Joined, Joiner};
use crate::listen::Binder;
use crate::members::LastBoot;
use crate::metrics::bind_node_sockets;
use crate::node_map::NodeMap;
use crate::raft_log::RUNS;
use crate::ram_drain::RamDrain;
use crate::rebuild::Background;
use crate::replication::{REPAIR_DIR, Registered, bind_node};
use crate::serve::{ServeError, StorageListener, StorageSocket};
use crate::start::{AbortOnDrop, open_cold_file};
use crate::startup_pass::StartPass;
use crate::stores::{FILE_PREFIX_ROOT, Hot, StoreUri, Stores};
use crate::write_tier::FLUSH_EVERY;
use crate::{Args, ConfigError, RaftMetaLog, SERVER_VERSION, StartFailure, interrupted, sizes_on};

/// The flags of [`Args`] a storage node takes, besides the thread flags
/// ([`kbf_threads::PoolArgs`]): where its bytes are and how they are written.
pub const ALLOWED: &[&str] = &[
    "cold_store",
    "cold_store_credentials_file",
    "hot_store",
    "hot_store_max_bytes",
    "hot_store_abandon",
    "meta_dir",
    "cold_store_floor_bytes",
    "cold_store_size_bytes",
    "cold_store_evict_grace_ms",
    "log_dir",
    "log_max_bytes",
    "log_files",
    "segment_upload_max_bytes",
    "segment_upload_idle_secs",
    "write_linger",
    "write_own_segment_bytes",
    "write_bytes_in_flight",
    "tls_reload_interval_secs",
    "write_puts_in_flight",
    "write_pool_writers",
    "write_pool_queue",
    "write_pool_stream_idle",
    "durable_wait",
    "ram_peer_grace",
    "ram_ack_bytes",
    "ram_drain_deadline",
    "store_put_attempts",
];

/// The id of `--storage-listen`.
const LISTEN: &str = "storage_listen";
/// The ids of the registration flags.
const REGISTRATION: [&str; 4] = [
    "frontend",
    "advertise_addr",
    "storage_zone",
    "storage_weight",
];
/// The id of `--metrics-listen`.
const METRICS: &str = "metrics_listen";
/// The ids of the three TLS flags, in the order [`StorageFlags::tls`] reads them.
const TLS: [&str; 3] = ["storage_tls_cert", "storage_tls_key", "storage_client_ca"];

/// The storage node's flags, from one command line.
#[derive(Clone, Debug)]
pub struct StorageFlags {
    /// The flags of `kbf-server` the node takes; every other field of it is its default
    /// and unused.
    pub server: Args,
    /// Where the node listens.
    pub listen: SocketAddr,
    /// Where the node serves `GET /metrics` over plain HTTP, if anywhere.
    pub metrics: Option<SocketAddr>,
    /// The server certificate, its key and the client CA, all three or none.
    pub tls: Option<[PathBuf; 3]>,
    /// The frontend to register with (`http://` or `https://`), if any.
    pub frontend: Option<String>,
    /// The address to tell the frontend callers dial; default the bound address.
    pub advertise: Option<String>,
    /// The zone label to register with.
    pub zone: String,
    /// The weight to register with.
    pub weight: u32,
    /// How the repair worker of a node that registered with a frontend is paced.
    pub repair: RepairConfig,
    /// Where the S3 key pair comes from (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`):
    /// the process environment, unless a test says otherwise.
    pub env: fn(&str) -> Option<String>,
}

/// The process environment, as [`StorageFlags::env`] reads it.
#[must_use]
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The long names of the flags of [`Args`] that `kbf storage` hides and refuses by name
/// ([`parse`]): every flag outside [`ALLOWED`] and the thread flags.
#[must_use]
pub fn refused_flags() -> std::collections::BTreeSet<String> {
    crate::flag_set::refused_names(ALLOWED)
}

/// The command line of `kbf storage`: [`Args`]'s, those it takes shown and the rest
/// hidden (so [`parse`] can refuse them by name), and the node's own.
#[must_use]
pub fn command() -> Command {
    let cmd = crate::flag_set::command(
        "storage",
        "The storage node: the cache, served over gRPC to frontends and daemons.",
        ALLOWED,
    );
    let path =
        |id: &'static str, long: &'static str, requires: [&'static str; 2], help: &'static str| {
            Arg::new(id)
                .long(long)
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires_all(requires)
                .help(help)
        };
    cmd.arg(
        Arg::new(LISTEN)
            .long("storage-listen")
            .value_name("ADDR")
            .default_value("127.0.0.1:8982")
            .value_parser(clap::value_parser!(SocketAddr))
            .help(
                "Where the node serves gRPC. Without the TLS flags it serves plain text, and \
                 the address must be loopback.",
            ),
    )
    .arg(
        Arg::new(METRICS)
            .long("metrics-listen")
            .value_name("ADDR")
            .value_parser(clap::value_parser!(SocketAddr))
            .help(
                "Serves GET /metrics (Prometheus text) over plain HTTP here. Off unless given. \
                 Reads are open to whoever reaches it: bind it where only operators do.",
            ),
    )
    .arg(path(
        TLS[0],
        "storage-tls-cert",
        [TLS[1], TLS[2]],
        "The node's server certificate (PEM).",
    ))
    .arg(path(
        TLS[1],
        "storage-tls-key",
        [TLS[0], TLS[2]],
        "The server certificate's private key (PEM).",
    ))
    .arg(
        Arg::new(REGISTRATION[0])
            .long("frontend")
            .value_name("URL")
            .help(
                "Registers with this frontend (http:// or https://) and heartbeats to it. \
                 Without it the node is a one-member map of itself.",
            ),
    )
    .arg(
        Arg::new(REGISTRATION[1])
            .long("advertise-addr")
            .value_name("HOST:PORT")
            .requires(REGISTRATION[0])
            .help(
                "The address callers dial, where --storage-listen is not routable. Default: \
                 the address the node bound.",
            ),
    )
    .arg(
        Arg::new(REGISTRATION[2])
            .long("storage-zone")
            .value_name("LABEL")
            .requires(REGISTRATION[0])
            .help("The zone label to register with. Default: none."),
    )
    .arg(
        Arg::new(REGISTRATION[3])
            .long("storage-weight")
            .value_name("N")
            .requires(REGISTRATION[0])
            .value_parser(clap::value_parser!(u32).range(1..))
            .help("The node's share of the digests relative to the others. Default: 1."),
    )
    .arg(path(
        TLS[2],
        "storage-client-ca",
        [TLS[0], TLS[1]],
        "The CA callers' certificates chain to; each must name a role (frontend, storage, daemon).",
    ))
}

/// The flags `m`, matched by [`command`], make.
///
/// # Errors
/// A flag of `kbf-server` that a storage node does not take was given (the error names
/// it; it is also how `--state-store` is refused, a flag no storage node has).
pub fn parse(m: &ArgMatches) -> Result<StorageFlags, clap::Error> {
    if let Some(flag) = crate::flag_set::refused(m, ALLOWED) {
        return Err(command().error(
            clap::error::ErrorKind::ArgumentConflict,
            format!(
                "kbf storage refuses --{flag}: a storage node serves blobs and the action \
                 cache and keeps no scheduler state; it takes the store, write and log \
                 flags (docs/design/architecture-v2.md, section 3)"
            ),
        ));
    }
    let server = Args::from_arg_matches(m)?;
    let paths: Vec<PathBuf> = TLS
        .iter()
        .filter_map(|id| m.get_one::<PathBuf>(id).cloned())
        .collect();
    let listen = *m
        .get_one::<SocketAddr>(LISTEN)
        .expect("--storage-listen has a default");
    // clap makes the three come together.
    let tls = <[PathBuf; 3]>::try_from(paths).ok();
    let text = |id: &str| m.get_one::<String>(id).cloned();
    let (frontend, advertise) = (text(REGISTRATION[0]), text(REGISTRATION[1]));
    if let Some(url) = &frontend {
        let secure = url.starts_with("https://");
        let refusal = if !secure && !url.starts_with("http://") {
            Some("--frontend must be an http:// or https:// URL")
        } else if secure && tls.is_none() {
            Some(
                "--frontend https:// needs --storage-tls-cert, --storage-tls-key and --storage-client-ca",
            )
        } else if advertise.is_none() && listen.ip().is_unspecified() {
            Some("--storage-listen is a wildcard address, so --advertise-addr is required")
        } else {
            None
        };
        if let Some(refusal) = refusal {
            return Err(command().error(clap::error::ErrorKind::InvalidValue, refusal));
        }
    }
    let repair = server.repair_config();
    Ok(StorageFlags {
        server,
        listen,
        metrics: m.get_one::<SocketAddr>(METRICS).copied(),
        tls,
        frontend,
        advertise,
        zone: text(REGISTRATION[2]).unwrap_or_default(),
        weight: m.get_one::<u32>(REGISTRATION[3]).copied().unwrap_or(1),
        repair,
        env: process_env,
    })
}

impl StorageFlags {
    /// The listener these flags describe.
    ///
    /// # Errors
    /// A TLS file cannot be read; `--segment-upload-max-bytes` is over
    /// `--write-bytes-in-flight`; there is no TLS and `--storage-listen` is not a
    /// loopback address ([`ConfigError::PlaintextStorageOffLoopback`]: plain text admits
    /// every call, so it is for one machine only).
    pub fn listener(&self) -> Result<StorageListener, ConfigError> {
        self.server.segment_upload_fits_write_bound()?;
        self.server.durable_wait_fits_commit_bound()?;
        let tls = match &self.tls {
            Some([cert, key, ca]) => Some(TlsSource::files(
                "storage",
                TlsFiles {
                    cert: cert.clone(),
                    key: key.clone(),
                    client_ca: Some(ca.clone()),
                },
                Duration::from_secs(self.server.tls_reload_interval_secs),
            )?),
            None if crate::config::loopback_only(self.listen.ip()) => None,
            None => {
                return Err(ConfigError::PlaintextStorageOffLoopback { addr: self.listen });
            }
        };
        Ok(StorageListener {
            listen: self.listen,
            tls,
            service: kbf_storage::service::Config {
                write_bytes_in_flight: u32::try_from(self.server.write_bytes_in_flight)
                    .unwrap_or(u32::MAX),
            },
        })
    }

    /// The certificate this node calls with and the CA it trusts, as the flags name them:
    /// for its registration with an `https://` frontend and for its calls to the other
    /// storage nodes; `None` without the TLS flags.
    ///
    /// # Errors
    /// A TLS file cannot be read.
    pub fn peer_tls(&self) -> Result<Option<ClientTlsConfig>, ConfigError> {
        let Some([cert, key, ca]) = &self.tls else {
            return Ok(None);
        };
        let identity = Identity::from_pem(crate::config::read(cert)?, crate::config::read(key)?);
        let ca = Certificate::from_pem(crate::config::read(ca)?);
        Ok(Some(
            ClientTlsConfig::new().identity(identity).ca_certificate(ca),
        ))
    }

    /// The address the node registers under, and so the one its member of the map
    /// carries: `--advertise-addr`, or the address it `bound`.
    #[must_use]
    pub fn registers_as(&self, bound: SocketAddr) -> String {
        self.advertise.clone().unwrap_or_else(|| bound.to_string())
    }

    /// The registration with `--frontend`, for a node bound at `bound`, keeping the maps it
    /// is sent in `node` and telling it which member it is; `None` without the flag.
    /// Connects lazily, so a frontend that is down at start is waited for.
    ///
    /// # Errors
    /// A TLS file cannot be read or is refused; the URL is refused.
    pub fn joiner(
        &self,
        bound: SocketAddr,
        node: Arc<NodeMap>,
    ) -> Result<Option<Joiner>, Box<dyn Error>> {
        let Some(url) = &self.frontend else {
            return Ok(None);
        };
        let mut endpoint = Endpoint::from_shared(url.clone())?;
        if let (true, Some(tls)) = (url.starts_with("https://"), self.peer_tls()?) {
            endpoint = endpoint.tls_config(tls)?;
        }
        let addr = self.registers_as(bound);
        let identity = join::Identity {
            instance_id: addr.clone(),
            addr,
            zone: self.zone.clone(),
            weight: self.weight,
        };
        let joiner = Joiner::new(endpoint.connect_lazy(), identity, join::RETRY_BASE);
        Ok(Some(joiner.with_node(node)))
    }

    /// Whether the node may hold the keys of a registered cold store: it registers with a
    /// frontend and its cold store is an `s3://` bucket, or `registered`. Such a node turns
    /// off core dumps before it starts ([`crate::cold_apply::harden_process`]).
    #[must_use]
    pub fn may_hold_keys(&self) -> bool {
        self.frontend.is_some()
            && matches!(
                self.server.cold_store,
                Some(StoreUri::S3(_) | StoreUri::Registered)
            )
    }

    /// The `Segments` uploads the node keeps: `--segment-upload-*`.
    fn segments(&self) -> SegmentsConfig {
        SegmentsConfig {
            max_bytes: self.server.segment_upload_max_bytes,
            idle: Duration::from_secs(self.server.segment_upload_idle_secs),
        }
    }
}

/// How long [`StorageHandle::serve`] waits for the cold store's lock after serving ends.
/// A stop that works finishes it in milliseconds (the abort of the cache's tasks and the
/// end of its writer threads); ten seconds is far past that and short of any supervisor's
/// patience. Past it the lock is left to the process's exit.
pub const COLD_RELEASE_DEADLINE: Duration = Duration::from_secs(10);

/// A started storage node: bound, its write tier attached and its flusher (and any
/// start-up pass) running, not yet serving.
pub struct StorageHandle {
    /// Where the node listens.
    pub addr: SocketAddr,
    /// Where it serves `/metrics`, when `--metrics-listen` is given.
    pub metrics: Option<SocketAddr>,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
    _flusher: Option<AbortOnDrop>,
    _background: Background,
    /// The flush report and tail passes a registered node's heartbeats carry ([`crate::fill`]).
    _fill: Option<AbortOnDrop>,
    /// When the cold store is a directory: completes when its lock is released.
    cold_released: Option<kbf_objstore::Released>,
    maps: Arc<MapHolder>,
    catch_up: CatchUp,
    repairs: Option<Arc<Repairs>>,
    _joiner: Option<AbortOnDrop>,
    _repair: Option<AbortOnDrop>,
    _metrics: Option<AbortOnDrop>,
    /// Holds a blob in this node's cache: [`StorageHandle::hold`].
    holder: Holder,
}

/// Holds a blob in a node's cache, or `None` once the cache is gone.
pub(crate) type Holder = Box<dyn Fn(&VerifiedBlob) -> Option<Hold> + Send + Sync>;

/// A [`Holder`] of `cache`, which it does not keep alive.
pub(crate) fn holder_of<M: MetaLog + 'static, O: ObjectStore + 'static>(
    cache: &Arc<Cache<M, O>>,
) -> Holder {
    let cache = Arc::downgrade(cache);
    Box::new(move |blob| cache.upgrade().map(|cache| cache.hold(blob)))
}

impl StorageHandle {
    /// Holds `blob` in this node's RAM without a write (`Cache::hold`), and says what
    /// became of it (`None`: the node's cache is gone). For the tests of the node's wiring
    /// (`--ram-ack-bytes` reaches the cache, and the stop drains what it holds); a write
    /// reaches the set through `StreamedPut::commit` with `Held`.
    #[doc(hidden)]
    pub fn hold(&self, blob: &VerifiedBlob) -> Option<Hold> {
        (self.holder)(blob)
    }

    /// The copies this node owes: its repair journal. `None` for a node that does not
    /// replicate (no `--frontend`, or one that admits callers in plain text).
    #[must_use]
    pub fn repairs(&self) -> Option<Arc<Repairs>> {
        self.repairs.clone()
    }

    /// The storage map this node holds: what the frontend sent it when it registered and
    /// at each heartbeat that found it behind. Empty for a node with no `--frontend`.
    #[must_use]
    pub fn maps(&self) -> Arc<MapHolder> {
        Arc::clone(&self.maps)
    }

    /// Whether the node is rebuilding its index from the store, which it does at a start
    /// over a metadata log that never finished a rebuild (a new or wiped one) when it
    /// registers with `--frontend`. It serves meanwhile, answering a miss UNAVAILABLE;
    /// [`CatchUp::finished`] completes when the rebuild is done.
    #[must_use]
    pub fn catch_up(&self) -> CatchUp {
        self.catch_up.clone()
    }

    /// The line the node prints once it is bound:
    /// `kbf-storage <version> storage=<addr> log=<id>`.
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// Serves until the shutdown future given to [`start_storage`] completes and the
    /// calls in flight end; no new call is taken once it has completed. It then writes
    /// the blobs held in RAM to the disk and flushes the blocks staged on the hot store to
    /// the cold store, both within `--ram-drain-deadline` (`RamDrain::after`); what the
    /// deadline leaves is logged, and is on the hot store's disk for the next start.
    /// With a `file://` cold store it then waits for the store's lock to be released, so a
    /// start over the same directories can follow at once: the node's tasks are aborted
    /// and the cache they and the service held is closed, but its own background tasks and
    /// writer threads end after it is dropped, not before. The wait is bounded by
    /// [`COLD_RELEASE_DEADLINE`]; past it this logs one warning (the store's directory and
    /// how many handles are alive) and returns, because the process's exit releases the
    /// lock whatever still holds it, and the units run this with `TimeoutStopSec=infinity`,
    /// where a wait with no end would be a stop with none.
    ///
    /// The other serve paths (`storage_half::serve`, `start::serve`) wait for nothing: the
    /// process exits when they return, and its exit frees the lock.
    ///
    /// The tasks that hold handles on the cold store, to look at when the warning shows:
    /// the cache's touch flusher (`kbf-front`, `cache/touches.rs`) and its write-pool
    /// threads (`cache/pool.rs`).
    ///
    /// # Errors
    /// The listener fails while serving.
    pub async fn serve(self) -> Result<(), ServeError> {
        let (served, released) = self.serve_checked(COLD_RELEASE_DEADLINE).await;
        if let Err(held) = released {
            tracing::warn!(
                dir = %held.dir.display(),
                clones = held.clones,
                limit = ?held.limit,
                "the cold store is still open after serving ended; its lock is released when the process exits"
            );
        }
        served
    }

    /// [`StorageHandle::serve`] without the warning, saying whether the cold store was
    /// released within `limit` (on the runtime's clock): what the tests call, so a
    /// handle leaked by the node fails a test instead of hanging it or passing unseen.
    /// The result of serving comes first.
    #[doc(hidden)]
    pub async fn serve_checked(
        self,
        limit: Duration,
    ) -> (Result<(), ServeError>, Result<(), StillHeld>) {
        let Self {
            addr: _,
            metrics: _,
            start_line: _,
            serving,
            _flusher,
            _background,
            _fill,
            cold_released,
            maps,
            catch_up,
            repairs,
            _joiner,
            _repair,
            _metrics,
            holder: _,
        } = self;
        let served = serving.await;
        // Everything the handle holds goes, so what is left holding the stores is not it.
        drop((
            _flusher,
            _background,
            _fill,
            maps,
            catch_up,
            repairs,
            _joiner,
            _repair,
            _metrics,
        ));
        let released = match cold_released {
            Some(released) => released.wait_within(limit).await,
            None => Ok(()),
        };
        (served, released)
    }
}

/// Opens the stores the flags name, as `kbf-server` does ([`crate::start`]), and binds
/// the storage listener over the cache.
///
/// # Errors
/// A flag the node refuses (the listener is checked first, before anything is opened:
/// [`StorageFlags::listener`]), a store or log that cannot be opened, a listener that
/// cannot be bound, a write tier that refuses to start, or a `shutdown` that fails.
pub async fn start_storage<S, F>(
    flags: &StorageFlags,
    shutdown: S,
) -> Result<StorageHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let listener = flags.listener()?;
    let stores = flags.server.stores(&Statvfs)?;
    flags.server.check_cold_key_file()?;
    for line in stores.describe() {
        tracing::info!("{line}");
    }
    match &stores.cold {
        StoreUri::File(dir) => {
            let store = open_cold_file(&flags.server, dir)?;
            let files = Some(store.clone());
            over(
                flags,
                listener,
                &stores,
                (Fixed(store), files),
                FILE_PREFIX_ROOT,
                shutdown,
            )
            .await
        }
        StoreUri::S3(uri) => {
            let cold = Registrable::new(
                uri,
                flags.server.cold_store_credentials_file.as_deref(),
                flags.env,
            )?;
            over(
                flags,
                listener,
                &stores,
                (cold, None),
                &uri.prefix,
                shutdown,
            )
            .await
        }
        StoreUri::Registered => {
            // The registration names the prefix the metadata log keeps its objects under,
            // so it is fetched before the log opens.
            let cold = RegisteredOnly::wait(flags).await?;
            let prefix = cold.prefix_root();
            over(flags, listener, &stores, (cold, None), &prefix, shutdown).await
        }
    }
}

/// [`start_storage`] over the object store the cold store's URI named.
async fn over<C, S, F>(
    flags: &StorageFlags,
    listener: StorageListener,
    stores: &Stores,
    (cold, files): (C, Option<kbf_objstore::FsStore>),
    prefix_root: &str,
    shutdown: S,
) -> Result<StorageHandle, Box<dyn Error>>
where
    C: ColdPick,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let args = &flags.server;
    let meta = RaftMetaLog::open_dir(&stores.meta_dir, Retention::default(), prefix_root).await?;
    let (prefix, log) = (meta.store_prefix(), meta.log_id());
    let grace = Duration::from_millis(args.cold_store_evict_grace_ms);
    let cold_released = files.as_ref().map(kbf_objstore::FsStore::released);
    let pass = files.map(|files| StartPass::new(files, grace));
    // After the log is open: it refuses a directory that holds files it did not write.
    let repair_dir = stores.meta_dir.join(REPAIR_DIR);
    assemble(
        flags,
        listener,
        stores.hot.as_ref(),
        (meta, cold, prefix, log),
        (shutdown, &repair_dir),
        (pass, cold_released),
        RebuildConfig::default(),
    )
    .await
}

/// The cache, its write tier, flusher and pass, and the listener over them. With
/// `--frontend` the listener is bound and the node registered before the cache opens,
/// so the cache writes as the registration's member and boot ([`open_registered`]).
async fn assemble<C, S, F>(
    flags: &StorageFlags,
    listener: StorageListener,
    hot: Option<&Hot>,
    (meta, cold, prefix, log): (RaftMetaLog<StdFs>, C, KeyPrefix, FarmLogId),
    (shutdown, repair_dir): (S, &Path),
    (pass, cold_released): (Option<StartPass>, Option<kbf_objstore::Released>),
    rebuild: RebuildConfig,
) -> Result<StorageHandle, Box<dyn Error>>
where
    C: ColdPick,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    // One start, one bind budget for both listeners, spent before the first await (a
    // `Binder` is not `Send`).
    let (socket, metrics) = bind_node_sockets(listener.listen, flags.metrics, &Binder::new())?;
    let cold_metrics = Arc::new(StoreMetrics::cold());
    let cold = CountedPick(cold, Arc::clone(&cold_metrics));
    let (cache, registered) = open_cache(flags, socket, (meta, cold, prefix, log)).await?;
    // The reads this node serves from its own copy: the service counts them, the scrape of
    // the cache renders them.
    let legacy_reads = Arc::new(LegacyReads::default());
    let cache = cache
        .with_legacy_reads(Arc::clone(&legacy_reads))
        .with_cold_store_metrics(cold_metrics)
        .with_writes(flags.server.write_config())
        .with_write_pool(flags.server.write_pool_config())
        .with_durable_wait(flags.server.durable_wait())
        .with_ram_ack_bytes(flags.server.ram_ack_bytes());
    // A registered node dates what it stages by the map it holds, so it can say what it has
    // flushed ([`crate::fill`]); it flushes everything, as without the gate.
    let cache = if registered.joiner.is_some() {
        cache.with_flush_gate(Arc::new(crate::fill::FlushesAll::new(Arc::clone(
            &registered.node,
        ))))
    } else {
        cache
    };
    let (cache, tier) = crate::write_tier::attach(cache, hot).await?;
    let cache = Arc::new(cache);
    let (metrics, _metrics) = metrics
        .map(|m| m.serve(Arc::clone(&cache), None, listener.tls.clone()))
        .unzip();
    let _flusher = tier.map(|_| {
        let flusher = crate::write_tier::run_flusher(Arc::clone(&cache), FLUSH_EVERY);
        AbortOnDrop(tokio::spawn(flusher))
    });
    let catch_up = registered.catch_up.clone();
    let _background = crate::rebuild::spawn(&cache, &catch_up, pass, rebuild);
    // The registration's heartbeats say what this reports: the flush, and the tail passes the
    // frontend's fence asks for.
    let _fill = registered.joiner.as_ref().map(|(joiner, joined)| {
        let fill = crate::fill::run(
            Arc::clone(&cache),
            joiner.fill(),
            Arc::clone(&registered.node),
            catch_up.clone(),
            joined.interval,
        );
        AbortOnDrop(tokio::spawn(fill))
    });
    let ram = RamDrain::start(
        Arc::clone(&cache),
        flags.server.ram_ack_bytes(),
        flags.server.ram_drain_deadline(),
    );
    let holder = holder_of(&cache);
    let storage = Arc::new(CacheStorage::new(cache, flags.segments()));
    let stop = Box::pin(shutdown()?);
    let node = bind_node(
        flags,
        listener,
        registered,
        storage,
        legacy_reads,
        repair_dir,
        stop,
    )?;
    Ok(StorageHandle {
        addr: node.addr,
        metrics,
        start_line: format!(
            "kbf-storage {SERVER_VERSION} storage={} log={log}",
            node.addr
        ),
        serving: Box::pin(ram.after(node.serving)),
        _flusher,
        _background,
        _fill,
        cold_released,
        maps: node.maps,
        catch_up,
        repairs: node.repairs,
        _joiner: node.joiner,
        _repair: node.repair,
        _metrics,
        holder,
    })
}

/// The cache of a node bound at `socket`. With `--frontend` the node registers first
/// (retrying until the frontend answers), so the cache writes as the registration's member
/// and boot ([`open_registered`]); the registration and what it gave come back for the
/// heartbeats ([`Joiner::keep`]). Without it the cache writes in an epoch of its own log.
///
/// The [`CatchUp`] in [`Registered`] says whether the node owes a rebuild of its index: a
/// registered node whose log has not finished one does, and tells the frontend so in its
/// registration, so the map ranks it last until it is done ([`catch_up_for`]). A node without
/// `--frontend` owes none: it keeps its objects under its log's own prefix, which no other log's loss
/// can orphan.
/// `kbf storage` and the storage half of `kbf storage-and-daemon` both open theirs here.
///
/// # Errors
/// A TLS file or the frontend URL is refused, or the cache cannot be opened
/// ([`open_registered`]).
pub(crate) async fn open_cache<C: ColdPick>(
    flags: &StorageFlags,
    socket: StorageSocket,
    (meta, cold, prefix, log): (RaftMetaLog<StdFs>, C, KeyPrefix, FarmLogId),
) -> Result<(Cache<RaftMetaLog<StdFs>, C::Store>, Registered), Box<dyn Error>> {
    let keys = ObjectKeys {
        root: prefix_root(&prefix, log)?,
        legacy: prefix,
    };
    let node = Arc::new(NodeMap::new(Arc::new(MapHolder::new())));
    Ok(match flags.joiner(socket.addr(), Arc::clone(&node))? {
        Some(joiner) => {
            let done = meta.query(|state| state.rebuild().is_done()).await?;
            let catch_up = catch_up_for(done);
            // The member and boot the log last wrote as: a store that counts from 1 gives
            // the node a boot above it, so the open below is not refused.
            let last = last_boot(&meta).await?;
            // What the node runs and what its log is at go to the frontend in the
            // registration and every heartbeat.
            let mut joiner = joiner
                .with_catch_up(catch_up.clone())
                .with_last_boot(last)
                .with_log(RUNS.get(), meta.version_handle());
            let joined = joiner.join().await;
            // The store is chosen after the registration and before the cache opens: the
            // keys the frontend registered are probed first (`crate::cold_apply`), and the
            // heartbeats report what became of them from the first one.
            let (store, applied) = cold.registered(&joiner, &joined).await?;
            let joiner = joiner.with_applied(applied);
            let cache = open_registered(meta, store, keys, &joined).await?;
            let joiner = Some((joiner, joined));
            (
                cache,
                Registered {
                    socket,
                    node,
                    joiner,
                    catch_up,
                },
            )
        }
        None => (
            Cache::open_under(meta, cold.alone()?, keys).await?,
            Registered {
                socket,
                node,
                joiner: None,
                catch_up: CatchUp::caught_up(),
            },
        ),
    })
}

/// Whether a registered node owes a rebuild of its index: its log has not finished one
/// (`done`). That is all it depends on: the rebuild's commands (`AdoptSegment`,
/// `RebuildProgress`, `RebuildDone`) commit to a log at any machine version, so the
/// log's version says nothing about whether the rebuild can run.
fn catch_up_for(done: bool) -> CatchUp {
    if done {
        CatchUp::caught_up()
    } else {
        CatchUp::rebuilding()
    }
}

/// The member and boot `meta` last wrote as (the node writer it set last), which the node
/// tells the frontend when it registers; `None` for a log that has set none.
///
/// # Errors
/// The log cannot be read.
async fn last_boot(meta: &RaftMetaLog<StdFs>) -> Result<Option<LastBoot>, MetaLogError> {
    let writer = meta.query(|state| state.current_writer()).await?;
    Ok(writer.and_then(|w| LastBoot::from_wire(w.member().get(), w.boot().get())))
}

/// The prefix a node writer's keys go under: the log's key prefix, `<root><log id>/`,
/// less its log id, so both kinds of key share the root the log was created with.
///
/// # Errors
/// The prefix does not end in the log's id (a log this binary did not create).
fn prefix_root(prefix: &KeyPrefix, log: FarmLogId) -> Result<KeyPrefix, Box<dyn Error>> {
    let root = prefix
        .as_str()
        .strip_suffix(&format!("{log}/"))
        .ok_or_else(|| {
            let prefix = prefix.as_str();
            format!("the log's key prefix {prefix} does not end in its id {log}")
        })?;
    Ok(KeyPrefix::new(root.to_owned())?)
}

/// The cache of a node registered as `joined`: the node writer set on its log as the log
/// is, whatever machine version it holds, so every object it writes is named by
/// `joined`'s member and boot.
///
/// # Errors
/// The registration names a zero member or boot; the log has more than one voter, or has
/// set this boot of the member already (a state store that handed the boot out again:
/// [`kbf_front::CacheError::StaleWriter`]).
pub(crate) async fn open_registered<O: ObjectStore>(
    meta: RaftMetaLog<StdFs>,
    store: O,
    keys: ObjectKeys,
    joined: &Joined,
) -> Result<Cache<RaftMetaLog<StdFs>, O>, Box<dyn Error>> {
    let (member, boot) = (
        NonZeroU64::new(joined.member_id),
        NonZeroU64::new(joined.boot),
    );
    let (Some(member), Some(boot)) = (member, boot) else {
        let (m, b) = (joined.member_id, joined.boot);
        return Err(format!(
            "the frontend registered this node as member {m} boot {b}; neither may be zero"
        )
        .into());
    };
    let voters = meta.voters();
    open_among(meta, store, keys, (member, boot), voters).await
}

/// [`open_registered`] for a node named `(member, boot)`, over a log of `voters` voters.
async fn open_among<O: ObjectStore>(
    meta: RaftMetaLog<StdFs>,
    store: O,
    keys: ObjectKeys,
    (member, boot): (NonZeroU64, NonZeroU64),
    voters: usize,
) -> Result<Cache<RaftMetaLog<StdFs>, O>, Box<dyn Error>> {
    one_voter(voters)?;
    Ok(Cache::open_node(meta, store, keys, NodeWriter::new(member, boot)).await?)
}

/// Refuses a log of more than one voter for a registered node: a replica that does not
/// know `SetWriter` would skip it and lose it.
fn one_voter(voters: usize) -> Result<(), String> {
    if voters > 1 {
        return Err(format!(
            "a registered storage node's metadata log has {voters} voters; it must have one"
        ));
    }
    Ok(())
}

/// `kbf storage`: the logs, a runtime the thread flags size, the node's start line on
/// stdout, and serving until SIGINT or SIGTERM. Exit 2 for a start the node refuses, 75
/// for one that can succeed on a later try ([`crate::StartFailure`]).
#[must_use]
pub fn run(flags: &StorageFlags) -> ExitCode {
    crate::log_file::init(&flags.server);
    match harden(flags).and_then(|()| serve(flags)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprint!("{}", crate::start_said("kbf storage", e.as_ref()));
            StartFailure::of(e.as_ref()).code()
        }
    }
}

/// Turns off core dumps for a node that may hold registered keys.
///
/// # Errors
/// The process cannot be hardened: the node does not start holding keys it cannot protect.
pub fn harden(flags: &StorageFlags) -> Result<(), Box<dyn Error>> {
    if flags.may_hold_keys() {
        crate::cold_apply::harden_process()?;
    }
    Ok(())
}

fn serve(flags: &StorageFlags) -> Result<(), Box<dyn Error>> {
    let sizes = sizes_on(&flags.server, Topology::detect());
    sizes.log();
    sizes.runtime()?.block_on(async {
        let node = start_storage(flags, interrupted).await?;
        println!("{}", node.start_line());
        Ok(node.serve().await?)
    })
}

#[cfg(test)]
#[path = "storage_node_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "storage_rebuild_tests.rs"]
mod rebuild_tests;

#[cfg(test)]
#[path = "storage_release_tests.rs"]
mod release_tests;
