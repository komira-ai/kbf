//! The start of a build node: a [`LocalCache`] in front of one upstream, served by the S3 front
//! as a read cache.
//!
//! The settings come from a file of the build-node shape (see
//! [`config_cache`](super::config_cache)). Before the address is bound the start asks the file
//! system under the cache the questions the layout stands on (see [`check_local`]; a file system
//! that answers one wrongly is exit status 3, as for a tier that fails its self-test), and asks
//! the upstream a HEAD of a key that is not there, which it answers with 404 when it is up and
//! the key is accepted (an upstream that does not answer is exit status 1: a restart may mend
//! it). The build node's upstream is read with a read-only key, so the write checks of the
//! storage node's self-test are not run against it.
//!
//! On SIGTERM or SIGINT the process drains as a storage node does; the eviction pass is the only
//! thing running beside the server, and it stops with it. Nothing is owed to the upstream: no
//! journal, no lease, no copy in flight.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::{Backend, Clock, Error as BackendError, SystemClock};

use super::cli::{Error, S3Flags, open_tier, probe_run_id, signals};
use super::config_cache::CacheSettings;
use super::front::server_config;
use super::local_cache::{CacheConfig, Disk, LocalCache, SystemDisk, check_local};
use super::{Drained, Server};

/// How often the eviction pass runs beside the server.
const PASS_INTERVAL: Duration = Duration::from_secs(10);

/// Asks the upstream a HEAD of a key that is not there: any answer but 404 is a problem.
async fn reach(upstream: &dyn Backend, run: &str, spec: &str) -> Result<(), Error> {
    match upstream.head(&format!("cache-probe/{run}")).await {
        Ok(_) | Err(BackendError::NotFound(_)) => Ok(()),
        Err(why) => {
            Err(format!("cold tier {spec}: the upstream did not answer a HEAD: {why}").into())
        }
    }
}

/// Opens the cache in `config`, checks its file system and the upstream, and says on stderr what
/// the start found.
async fn open(
    config: &CacheConfig,
    upstream: &Arc<dyn Backend>,
    disk: &Arc<dyn Disk>,
    run: &str,
) -> Result<LocalCache, Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (upstream, disk) = (Arc::clone(upstream), Arc::clone(disk));
    let opened = LocalCache::open(config.clone(), upstream, Arc::clone(&disk), clock);
    let cache = opened.map_err(|why| {
        format!(
            "opening the local cache at {}: {why}",
            config.path.display()
        )
    })?;
    check_local(&*disk, cache.path(), run)?;
    let (bytes, objects) = cache.held();
    eprintln!(
        "tierstore: local cache at {}: {objects} objects, {bytes} bytes, {} files removed at start",
        config.path.display(),
        cache.counters().discarded_at_start()
    );
    Ok(cache)
}

/// Serves `cache` until `stop` resolves, and then drains.
///
/// # Errors
/// A file system or an upstream that fails the check of the start, or an address that cannot be
/// bound.
pub(super) async fn serve_cache(
    settings: &CacheSettings,
    upstream: Arc<dyn Backend>,
    disk: Arc<dyn Disk>,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(SocketAddr),
) -> Result<Drained, Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut config = server_config(&settings.front, Arc::clone(&clock))?;
    let run = probe_run_id(&settings.front.proxy_id, clock.now(), std::process::id());
    let cache = open(&settings.cache, &upstream, &disk, &run).await?;
    reach(&*upstream, &run, &settings.upstream.spec).await?;
    if let Some(rate) = settings.scrub_rate {
        eprintln!("tierstore: scrub_rate {rate} B/s is accepted; this release does not scrub");
    }
    config.cache = Some(cache.clone());
    let backend: Arc<dyn Backend> = Arc::new(cache.clone());
    let server = Server::new(backend, config);
    let address = settings.front.listen;
    let bound = TcpListener::bind(address).await;
    let listener = bound.map_err(|why| format!("binding {address}: {why}"))?;
    ready(listener.local_addr()?);
    let stopping = Arc::clone(&server);
    let watcher = tokio::spawn(async move {
        stop.await;
        stopping.begin_drain();
    });
    let halt = Arc::new(Notify::new());
    let halting = Arc::clone(&halt);
    let serving = async move {
        let drained = server.serve(listener).await;
        halting.notify_one();
        drained
    };
    let maintaining = cache.maintain(PASS_INTERVAL, halt);
    let (drained, ()) = tokio::join!(serving, maintaining);
    watcher.abort();
    Ok(drained)
}

/// Opens the upstream the settings name and serves the cache in front of it until `stop`
/// resolves.
///
/// # Errors
/// As [`serve_cache`], and an upstream the settings do not make.
pub(super) async fn run_cache(
    settings: &CacheSettings,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(SocketAddr),
) -> Result<Drained, Error> {
    if settings.s3_request_timeout.is_zero() {
        return Err("--s3-request-timeout must be longer than zero".into());
    }
    let tier = &settings.upstream;
    let s3 = S3Flags {
        endpoint: tier.endpoint.as_deref(),
        region: tier.region.as_deref(),
        credentials: tier.credentials_file.as_deref(),
    };
    let upstream = open_tier("--cold", &tier.spec, &s3, settings.s3_request_timeout)?;
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    serve_cache(settings, upstream, disk, stop, ready).await
}

/// Runs the cache until SIGTERM or SIGINT.
pub(super) async fn serve_cache_until_signalled(settings: CacheSettings) -> Result<(), Error> {
    let stop = signals()?;
    let version = env!("CARGO_PKG_VERSION");
    let announce = |addr: SocketAddr| println!("tierstore {version} listen={addr} mode=read-cache");
    let drained = run_cache(&settings, stop, announce).await?;
    eprint!("{}", super::cli::cut_note(drained));
    Ok(())
}
