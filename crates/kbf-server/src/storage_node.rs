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
//! role in each caller's certificate. Without them the node serves plain text, for
//! tests and trials on one machine; the start line warns about a listener off loopback.
//!
//! Every other flag of `kbf-server` is refused by name ([`parse`]), computed from
//! [`Args`]: a flag added there that is not listed in [`ALLOWED`] is refused here with
//! no edit.
//!
//! **The map** (section 6) is one member: this node. Every request carries a generation
//! and the node ignores it; membership, `STALE_MAP` and the other members are av2-08.

use std::collections::BTreeSet;
use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::parser::ValueSource;
use clap::{Arg, ArgMatches, Command, CommandFactory as _, FromArgMatches as _};
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_log::FarmLogId;
use kbf_meta::Retention;
use kbf_objstore::{KeyPrefix, ObjectStore};
use kbf_store::StdFs;
use kbf_threads::Topology;
use kbf_threads::disk::Statvfs;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use crate::serve::{ServeError, StorageListener, bind_storage};
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

/// The ids of the thread flags, which `kbf-server` flattens into [`Args`].
fn thread_flags() -> BTreeSet<clap::Id> {
    let pool = <kbf_threads::PoolArgs as clap::Args>::augment_args(Command::new("pool"));
    pool.get_arguments().map(|a| a.get_id().clone()).collect()
}

/// Whether the storage node takes `id` of [`Args`].
fn takes(id: &clap::Id, threads: &BTreeSet<clap::Id>) -> bool {
    ALLOWED.contains(&id.as_str()) || threads.contains(id)
}

/// The id of `--storage-listen`.
const LISTEN: &str = "storage_listen";
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
}

/// The command line of `kbf storage`: [`Args`]'s, those it takes shown and the rest
/// hidden (so [`parse`] can refuse them by name), and the node's own.
#[must_use]
pub fn command() -> Command {
    let threads = thread_flags();
    let mut cmd = Command::new("storage")
        .about("The storage node: the cache, served over gRPC to frontends and daemons.");
    for arg in Args::command().get_arguments() {
        let shown = takes(arg.get_id(), &threads);
        cmd = cmd.arg(arg.clone().hide(!shown));
    }
    let path = |id: &'static str, long: &'static str, requires: [&'static str; 2], help: &'static str| {
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
            .help("Where the node serves gRPC (plain text unless the TLS flags are given)."),
    )
    .arg(path(TLS[0], "storage-tls-cert", [TLS[1], TLS[2]], "The node's server certificate (PEM)."))
    .arg(path(TLS[1], "storage-tls-key", [TLS[0], TLS[2]], "The server certificate's private key (PEM)."))
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
    let threads = thread_flags();
    for arg in Args::command().get_arguments() {
        let id = arg.get_id();
        if !takes(id, &threads) && m.value_source(id.as_str()) == Some(ValueSource::CommandLine) {
            let flag = arg.get_long().unwrap_or_else(|| id.as_str());
            return Err(command().error(
                clap::error::ErrorKind::ArgumentConflict,
                format!(
                    "kbf storage refuses --{flag}: a storage node serves blobs and the action \
                     cache and keeps no scheduler state; it takes the store, write and log \
                     flags (docs/design/architecture-v2.md, section 3)"
                ),
            ));
        }
    }
    let server = Args::from_arg_matches(m)?;
    let paths: Vec<PathBuf> = TLS
        .iter()
        .filter_map(|id| m.get_one::<PathBuf>(id).cloned())
        .collect();
    Ok(StorageFlags {
        server,
        listen: *m
            .get_one::<SocketAddr>(LISTEN)
            .expect("--storage-listen has a default"),
        // clap makes the three come together.
        tls: <[PathBuf; 3]>::try_from(paths).ok(),
    })
}

impl StorageFlags {
    /// The listener these flags describe.
    ///
    /// # Errors
    /// A TLS file cannot be read; `--segment-upload-max-bytes` is over
    /// `--write-bytes-in-flight`.
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
            None => None,
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
}

impl StorageHandle {
    /// The line the node prints once it is bound:
    /// `kbf-storage <version> storage=<addr> log=<id>`, and ` warning=plaintext-off-loopback`
    /// when it serves plain text on an address that is not loopback.
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
            ..
        } = self;
        serving.await
    }
}

/// Opens the stores the flags name, as `kbf-server` does ([`crate::start`]), and binds
/// the storage listener over the cache.
///
/// # Errors
/// A flag the node refuses, a store or log that cannot be opened, a listener that
/// cannot be bound, a write tier that refuses to start, or a `shutdown` that fails.
pub async fn start_storage<S, F>(
    flags: &StorageFlags,
    shutdown: S,
) -> Result<StorageHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let stores = flags.server.stores(&Statvfs)?;
    for line in stores.describe() {
        tracing::info!("{line}");
    }
    match &stores.cold {
        StoreUri::File(dir) => {
            let store = open_cold_file(&flags.server, dir)?;
            let files = Some(store.clone());
            over(flags, &stores, (store, files), FILE_PREFIX_ROOT, shutdown).await
        }
        StoreUri::S3(uri) => {
            let store = s3_store(uri, |name| std::env::var(name).ok())?;
            over(flags, &stores, (store, None), &uri.prefix, shutdown).await
        }
    }
}

/// [`start_storage`] over the object store the cold store's URI named.
async fn over<O, S, F>(
    flags: &StorageFlags,
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
    assemble(flags, stores.hot.as_ref(), (meta, store, prefix, log), shutdown, pass).await
}

/// The cache, its write tier, flusher and pass, and the listener over them.
async fn assemble<O, S, F>(
    flags: &StorageFlags,
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
    let listener = flags.listener()?;
    let plain = listener.tls.is_none() && !crate::config::loopback_only(listener.listen.ip());
    let cache = Cache::open(meta, store, prefix)
        .await?
        .with_writes(flags.server.write_config());
    let (cache, tier) = crate::write_tier::attach(cache, hot).await?;
    let cache = Arc::new(cache);
    let _flusher = tier.map(|_| {
        let flusher = crate::write_tier::run_flusher(Arc::clone(&cache), FLUSH_EVERY);
        AbortOnDrop(tokio::spawn(flusher))
    });
    let _pass = pass.map(|pass| AbortOnDrop(crate::startup_pass::spawn(Arc::clone(&cache), pass)));
    let storage = Arc::new(CacheStorage::new(cache, flags.segments()));
    let bound = bind_storage(storage, listener, shutdown()?)?;
    let warning = if plain {
        " warning=plaintext-off-loopback"
    } else {
        ""
    };
    Ok(StorageHandle {
        addr: bound.addr,
        start_line: format!(
            "kbf-storage {SERVER_VERSION} storage={} log={log}{warning}",
            bound.addr
        ),
        serving: Box::pin(bound.serving),
        _flusher,
        _pass,
    })
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
