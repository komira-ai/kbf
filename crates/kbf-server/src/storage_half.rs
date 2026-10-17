//! The storage half of `kbf storage-and-daemon` (`docs/design/architecture-v2.md`,
//! sections 1 and 10): the storage node `kbf storage` runs, on the same flags
//! ([`StorageFlags`]), with the storage it serves also handed to the caller in-process
//! ([`StorageHalf::storage`]), so the daemon half of the same process reads and writes
//! it through calls, with no socket between.
//!
//! [`start`] opens the stores and binds the listener as
//! [`crate::storage_node::start_storage`] does, with two differences:
//! - **One cache.** When `flags.server.local_cache` is set (the daemon half's input
//!   cache shares the hot store's disk), the write tier keeps no flushed copies and its
//!   bytes are counted in that one cache's budget, as `kbf all-in-one`'s server half
//!   does (`kbf_storage::local_cache`).
//! - **The handle.** The [`Storage`] the listener serves is kept, for the daemon half.
//!
//! - **The map and the chain.** With `--frontend` the half registers and heartbeats as
//!   `kbf storage` does, holds the maps the frontend sends in a
//!   [`kbf_mapsync::MapHolder`] ([`StorageHalf::maps`]), and its service judges every call
//!   by that map, serving nothing until the first arrives; under mutual TLS it is a link of
//!   the chain that copies a write to the blob's second owner, with the repair worker that
//!   makes a copy owed ([`StorageHalf::repairs`]); all as [`crate::replication`] says. The
//!   storage the daemon half is handed in-process is the store itself, not the service:
//!   what it reads and writes there is not judged by the map and is not copied.
//!
//! The rest (the listener's checks, the stores, the metadata log, the flusher and the
//! start-up pass over a `file://` cold store) is `kbf storage`'s, step for step.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_mapsync::MapHolder;
use kbf_meta::Retention;
use kbf_objstore::{FsStore, ObjectStore};
use kbf_storage::Storage;
use kbf_storage::repair::Repairs;
use kbf_threads::disk::Statvfs;

use crate::replication::{REPAIR_DIR, bind_node};
use crate::serve::{ServeError, StorageListener};
use crate::start::{AbortOnDrop, open_cold_file, share_storage, tier_bytes};
use crate::startup_pass::StartPass;
use crate::storage_node::StorageFlags;
use crate::stores::{FILE_PREFIX_ROOT, StoreUri, Stores};
use crate::write_tier::FLUSH_EVERY;
use crate::{RaftMetaLog, SERVER_VERSION, s3_store};

/// A started storage half: bound, its write tier attached and its flusher (and any
/// start-up pass) running, not yet serving.
pub struct StorageHalf {
    /// Where the listener is.
    pub addr: SocketAddr,
    storage: Arc<dyn Storage>,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
    maps: Arc<MapHolder>,
    repairs: Option<Arc<Repairs>>,
    _flusher: Option<AbortOnDrop>,
    _pass: Option<AbortOnDrop>,
    _joiner: Option<AbortOnDrop>,
    _repair: Option<AbortOnDrop>,
}

impl StorageHalf {
    /// The storage map the node holds ([`crate::storage_node::StorageHandle::maps`]).
    #[must_use]
    pub fn maps(&self) -> Arc<MapHolder> {
        Arc::clone(&self.maps)
    }

    /// The copies this node owes: its repair journal. `None` for a node that does not
    /// replicate (no `--frontend`, or one that admits callers in plain text).
    #[must_use]
    pub fn repairs(&self) -> Option<Arc<Repairs>> {
        self.repairs.clone()
    }

    /// The storage the listener serves: the daemon half's, in-process.
    #[must_use]
    pub fn storage(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.storage)
    }

    /// The line the node prints once it is bound, as `kbf storage` prints it:
    /// `kbf-storage <version> storage=<addr> log=<id>`.
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// Serves until the shutdown future given to [`start`] completes and the calls in
    /// flight end; the flusher and the pass stop then.
    ///
    /// # Errors
    /// The listener fails while serving.
    pub async fn serve(self) -> Result<(), ServeError> {
        let Self {
            serving,
            _flusher,
            _pass,
            _joiner,
            _repair,
            ..
        } = self;
        serving.await
    }
}

/// Opens the stores `flags` name and binds the storage listener over the cache, as
/// `kbf storage` does, keeping the storage for the caller (the module docs).
///
/// # Errors
/// What [`crate::storage_node::start_storage`] refuses: the listener's flags (checked
/// first, before anything is opened), a store or log that cannot be opened, a listener
/// that cannot be bound, a write tier that refuses to start, or a `shutdown` that fails.
pub async fn start<S, F>(flags: &StorageFlags, shutdown: S) -> Result<StorageHalf, Box<dyn Error>>
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
            let opened = (store, files);
            over(flags, listener, &stores, opened, FILE_PREFIX_ROOT, shutdown).await
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

/// [`start`] over the object store the cold store's URI named: the metadata log, the
/// cache, its write tier, flusher and pass, and the listener over them.
async fn over<O, S, F>(
    flags: &StorageFlags,
    listener: StorageListener,
    stores: &Stores,
    (store, files): (O, Option<FsStore>),
    prefix_root: &str,
    shutdown: S,
) -> Result<StorageHalf, Box<dyn Error>>
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
    let cache = Cache::open(meta, store, prefix)
        .await?
        .with_writes(args.write_config());
    let local = args.local_cache.as_ref();
    let hot = stores.hot.as_ref();
    let (cache, tier) = crate::write_tier::attach_with(cache, hot, local.is_none()).await?;
    let cache = Arc::new(cache);
    share_storage(local, tier_bytes(Arc::downgrade(&cache)));
    let _flusher = tier.map(|_| {
        let flusher = crate::write_tier::run_flusher(Arc::clone(&cache), FLUSH_EVERY);
        AbortOnDrop(tokio::spawn(flusher))
    });
    let _pass = pass.map(|pass| AbortOnDrop(crate::startup_pass::spawn(Arc::clone(&cache), pass)));
    let segments = SegmentsConfig {
        max_bytes: args.segment_upload_max_bytes,
        idle: Duration::from_secs(args.segment_upload_idle_secs),
    };
    let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(cache, segments));
    // After the log is open: it refuses a directory that holds files it did not write.
    let repair_dir = stores.meta_dir.join(REPAIR_DIR);
    let node = bind_node(
        flags,
        listener,
        Arc::clone(&storage),
        &repair_dir,
        Box::pin(shutdown()?),
    )?;
    let addr = node.addr;
    Ok(StorageHalf {
        addr,
        storage,
        start_line: format!("kbf-storage {SERVER_VERSION} storage={addr} log={log}"),
        serving: node.serving,
        maps: node.maps,
        repairs: node.repairs,
        _flusher,
        _pass,
        _joiner: node.joiner,
        _repair: node.repair,
    })
}
