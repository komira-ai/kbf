//! `kbf storage`: the storage node (`docs/design/architecture-v2.md`, sections 3 and 13
//! P2). The cache the server has, the same stores and the same write tier, served over
//! gRPC by [`kbf_storage::service`] and nothing else: no REAPI, no scheduler, no
//! worker streams, no operator API, and no state store (the node reaches that only
//! through a frontend, section 6).
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
//! store. Every request still carries a generation and the node ignores it; `STALE_MAP`
//! and the map push are the next change.
//!
//! **Object names.** A registered node writes as its registration's member id and boot
//! counter (`kbf_meta::NodeWriter`): its metadata log is raised to machine version 4, it
//! commits `SetWriter`, and its objects are `<root>cas/<member>/<boot>/<seq>`, so a node
//! whose metadata directory was wiped never reuses a key (the state store hands each boot
//! out once). A node without `--frontend` names its objects in a writer epoch of its log,
//! `<root><log id>/cas/<epoch>/<seq>`, as a server does. Either reads both kinds.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Arg, ArgMatches, Command, FromArgMatches as _};
use kbf_front::{Cache, CacheStorage, ObjectKeys, SegmentsConfig};
use kbf_log::{FarmLogId, MachineVersion};
use kbf_meta::{NodeWriter, Retention};
use kbf_objstore::{KeyPrefix, ObjectStore};
use kbf_store::StdFs;
use kbf_threads::Topology;
use kbf_threads::disk::Statvfs;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig};

use crate::join::{self, Joined, Joiner};
use crate::serve::{ServeError, StorageListener, bind_storage_socket, serve_storage};
use crate::start::{AbortOnDrop, open_cold_file};
use crate::startup_pass::StartPass;
use crate::stores::{FILE_PREFIX_ROOT, Hot, StoreUri, Stores};
use crate::write_tier::FLUSH_EVERY;
use crate::{Args, ConfigError, RaftMetaLog, SERVER_VERSION, interrupted, s3_store, sizes_on};

/// The flags of [`Args`] a storage node takes, besides the thread flags
/// ([`kbf_threads::PoolArgs`]): where its bytes are and how they are written.
pub const ALLOWED: &[&str] = &[
    "cold_store",
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
    "write_puts_in_flight",
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
    Ok(StorageFlags {
        server,
        listen,
        tls,
        frontend,
        advertise,
        zone: text(REGISTRATION[2]).unwrap_or_default(),
        weight: m.get_one::<u32>(REGISTRATION[3]).copied().unwrap_or(1),
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
        let tls = match &self.tls {
            Some([cert, key, ca]) => Some(
                ServerTlsConfig::new()
                    .identity(Identity::from_pem(
                        crate::config::read(cert)?,
                        crate::config::read(key)?,
                    ))
                    .client_ca_root(Certificate::from_pem(crate::config::read(ca)?)),
            ),
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

    /// The registration with `--frontend`, for a node bound at `bound`; `None` without the
    /// flag. Connects lazily, so a frontend that is down at start is waited for.
    ///
    /// # Errors
    /// A TLS file cannot be read or is refused; the URL is refused.
    pub fn joiner(&self, bound: SocketAddr) -> Result<Option<Joiner>, Box<dyn Error>> {
        let Some(url) = &self.frontend else {
            return Ok(None);
        };
        let mut endpoint = Endpoint::from_shared(url.clone())?;
        if let (true, Some([cert, key, ca])) = (url.starts_with("https://"), &self.tls) {
            let tls = ClientTlsConfig::new()
                .identity(Identity::from_pem(
                    crate::config::read(cert)?,
                    crate::config::read(key)?,
                ))
                .ca_certificate(Certificate::from_pem(crate::config::read(ca)?));
            endpoint = endpoint.tls_config(tls)?;
        }
        let addr = self.advertise.clone().unwrap_or_else(|| bound.to_string());
        let identity = join::Identity {
            instance_id: addr.clone(),
            addr,
            zone: self.zone.clone(),
            weight: self.weight,
        };
        Ok(Some(Joiner::new(
            endpoint.connect_lazy(),
            identity,
            join::RETRY_BASE,
        )))
    }

    /// The `Segments` uploads the node keeps: `--segment-upload-*`.
    fn segments(&self) -> SegmentsConfig {
        SegmentsConfig {
            max_bytes: self.server.segment_upload_max_bytes,
            idle: Duration::from_secs(self.server.segment_upload_idle_secs),
        }
    }
}

/// A started storage node: bound, its write tier attached and its flusher (and any
/// start-up pass) running, not yet serving.
pub struct StorageHandle {
    /// Where the node listens.
    pub addr: SocketAddr,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
    _flusher: Option<AbortOnDrop>,
    _pass: Option<AbortOnDrop>,
    _joiner: Option<AbortOnDrop>,
}

impl StorageHandle {
    /// The line the node prints once it is bound:
    /// `kbf-storage <version> storage=<addr> log=<id>`.
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// Serves until the shutdown future given to [`start_storage`] completes and the
    /// calls in flight end.
    ///
    /// # Errors
    /// The listener fails while serving.
    pub async fn serve(self) -> Result<(), ServeError> {
        let Self {
            serving,
            _flusher,
            _pass,
            _joiner,
            ..
        } = self;
        serving.await
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
                (store, files),
                FILE_PREFIX_ROOT,
                shutdown,
            )
            .await
        }
        StoreUri::S3(uri) => {
            let store = s3_store(uri, |name| std::env::var(name).ok())?;
            over(
                flags,
                listener,
                &stores,
                (store, None),
                &uri.prefix,
                shutdown,
            )
            .await
        }
    }
}

/// [`start_storage`] over the object store the cold store's URI named.
async fn over<O, S, F>(
    flags: &StorageFlags,
    listener: StorageListener,
    stores: &Stores,
    (store, files): (O, Option<kbf_objstore::FsStore>),
    prefix_root: &str,
    shutdown: S,
) -> Result<StorageHandle, Box<dyn Error>>
where
    O: ObjectStore + 'static,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let args = &flags.server;
    let meta = RaftMetaLog::open_dir(&stores.meta_dir, Retention::default(), prefix_root).await?;
    let (prefix, log) = (meta.store_prefix(), meta.log_id());
    let grace = Duration::from_millis(args.cold_store_evict_grace_ms);
    let pass = files.map(|files| StartPass::over_raft(files, grace));
    assemble(
        flags,
        listener,
        stores.hot.as_ref(),
        (meta, store, prefix, log),
        shutdown,
        pass,
    )
    .await
}

/// The cache, its write tier, flusher and pass, and the listener over them. With
/// `--frontend` the listener is bound and the node registered before the cache opens,
/// so the cache writes as the registration's member and boot ([`open_registered`]).
async fn assemble<O, S, F>(
    flags: &StorageFlags,
    listener: StorageListener,
    hot: Option<&Hot>,
    (meta, store, prefix, log): (RaftMetaLog<StdFs>, O, KeyPrefix, FarmLogId),
    shutdown: S,
    pass: Option<StartPass<RaftMetaLog<StdFs>>>,
) -> Result<StorageHandle, Box<dyn Error>>
where
    O: ObjectStore + 'static,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let socket = bind_storage_socket(listener)?;
    let keys = ObjectKeys {
        root: prefix_root(&prefix, log)?,
        legacy: prefix,
    };
    let (cache, joiner) = match flags.joiner(socket.addr())? {
        Some(mut joiner) => {
            let joined = joiner.join().await;
            let cache = open_registered(meta, store, keys, &joined).await?;
            (cache, Some((joiner, joined)))
        }
        None => (Cache::open_under(meta, store, keys).await?, None),
    };
    let cache = cache.with_writes(flags.server.write_config());
    let (cache, tier) = crate::write_tier::attach(cache, hot).await?;
    let cache = Arc::new(cache);
    let _flusher = tier.map(|_| {
        let flusher = crate::write_tier::run_flusher(Arc::clone(&cache), FLUSH_EVERY);
        AbortOnDrop(tokio::spawn(flusher))
    });
    let _pass = pass.map(|pass| AbortOnDrop(crate::startup_pass::spawn(Arc::clone(&cache), pass)));
    let storage = Arc::new(CacheStorage::new(cache, flags.segments()));
    let bound = serve_storage(storage, socket, shutdown()?)?;
    let _joiner = joiner.map(|(joiner, joined)| AbortOnDrop(tokio::spawn(joiner.keep(joined))));
    Ok(StorageHandle {
        addr: bound.addr,
        start_line: format!(
            "kbf-storage {SERVER_VERSION} storage={} log={log}",
            bound.addr
        ),
        serving: Box::pin(bound.serving),
        _flusher,
        _pass,
        _joiner,
    })
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

/// The cache of a node registered as `joined`: its log raised to machine version 4 (a
/// storage node's log has one voter, so no other replica must run it first) and the
/// node writer set, so every object it writes is named by `joined`'s member and boot.
///
/// # Errors
/// The registration names a zero member or boot; the log has more than one voter, will
/// not be raised, or has set this boot of the member already (a state store that handed
/// the boot out again: [`kbf_front::CacheError::StaleWriter`]).
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
    one_voter(meta.voters())?;
    meta.raise_machine_version(MachineVersion::V4)?;
    Ok(Cache::open_node(meta, store, keys, NodeWriter::new(member, boot)).await?)
}

/// Refuses a log of more than one voter for a registered node: raising it to machine
/// version 4 would make every replica that does not run it refuse the log.
fn one_voter(voters: usize) -> Result<(), String> {
    if voters > 1 {
        return Err(format!(
            "a registered storage node's metadata log has {voters} voters; it must have one"
        ));
    }
    Ok(())
}

/// `kbf storage`: the logs, a runtime the thread flags size, the node's start line on
/// stdout, and serving until SIGINT or SIGTERM. Exit 2 for a start the node refuses.
#[must_use]
pub fn run(flags: &StorageFlags) -> ExitCode {
    crate::log_file::init(&flags.server);
    match serve(flags) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf storage: {e}");
            ExitCode::from(2)
        }
    }
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
