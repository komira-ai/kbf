//! The start of a build node: a [`LocalCache`] in front of one upstream, served by the S3 front
//! as a read cache, or as a cache that writes through when the file says `write_through`.
//!
//! The settings come from a file of the build-node shape (see
//! [`config_cache`](super::config_cache)). Before the address is bound the start asks the file
//! system under the cache the questions the layout stands on (see [`check_local`]; a file system
//! that answers one wrongly is exit status 3, as for a tier that fails its self-test), and asks
//! the upstream a HEAD of a key that is not there, which it answers with 404 when it is up and
//! the key is accepted (an upstream that does not answer is exit status 1: a restart may mend
//! it). A `budget` above the size of the file system less the `free_floor` is refused as
//! flags that cannot go together (exit status 2); a file system whose size cannot be read, or
//! that reports 0 bytes, is not judged: the start says so on stderr and goes on.
//!
//! Without `write_through` the build node's upstream is read with a read-only key, so the write
//! checks of the storage node's self-test are not run against it. With it the node writes to the
//! upstream, and the start runs the whole self-test there (a create-only put that refuses a key
//! that exists, a multipart completion that does the same, and the reads the checks rest on)
//! under probe keys in `_tiering/selftest/`, which it deletes. An upstream that answers one of
//! them wrongly, a key that may not write included, is exit status 3; one that does not answer is
//! exit status 1, after the one try that the check of the start gives it.
//!
//! An S3 upstream is a list of one or more endpoints (see [`endpoints`](super::endpoints)), all
//! taken to be ready at the start (the HEAD above is the start's check), and a poll of every
//! endpoint's `/readyz` runs beside the server, the first one after one interval, until it
//! stops. The cache's own `/readyz` is 503 while no endpoint is usable.
//!
//! On SIGTERM or SIGINT the process drains as a storage node does; the eviction pass is the only
//! thing running beside the server, and it stops with it. Nothing is owed to the upstream: no
//! journal, no lease, no copy in flight.

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::{Backend, Clock, Error as BackendError, SelfTestOptions, SystemClock, self_test};

use super::cli::{Error, S3Flags, open_tier, probe_run_id, signals};
use super::config_cache::CacheSettings;
use super::endpoints::{Endpoints, HttpProbe};
use super::front::{InvalidFlags, server_config};
use super::local_cache::{CacheConfig, Disk, LocalCache, SystemDisk, check_local};
use super::{Drained, Server};

/// How often the eviction pass runs beside the server.
const PASS_INTERVAL: Duration = Duration::from_secs(10);

/// How long a poll of an endpoint's `/readyz` may take, connect included.
const READYZ_TIMEOUT: Duration = Duration::from_secs(2);

/// Asks the upstream a HEAD of a key that is not there: any answer but 404 is a problem.
async fn reach(upstream: &dyn Backend, run: &str, spec: &str) -> Result<(), Error> {
    match upstream.head(&format!("cache-probe/{run}")).await {
        Ok(_) | Err(BackendError::NotFound(_)) => Ok(()),
        Err(why) => {
            Err(format!("cold tier {spec}: the upstream did not answer a HEAD: {why}").into())
        }
    }
}

/// Asks the upstream the questions of the self-test, all of them: the node is about to write
/// to it. A key that may not write, or a store whose create-only answers are wrong, is a
/// violation (exit status 3); an upstream that cannot be asked is exit status 1. The upstream
/// has just answered `reach`, so the checks are tried once each.
async fn test_upstream(upstream: &dyn Backend, run: &str, spec: &str) -> Result<(), Error> {
    let name = format!("cold tier {spec}");
    let options = SelfTestOptions {
        name: &name,
        run_id: run,
        multipart: true,
        skip: &[],
        wait: Duration::ZERO,
        pause: Duration::from_secs(1),
        max_pause: Duration::from_secs(30),
    };
    let note = |line: String| eprintln!("tierstore: {line}");
    eprintln!("tierstore: testing the {name} for writes");
    self_test(upstream, &options, &note).await?;
    eprintln!("tierstore: tested the {name}");
    Ok(())
}

/// The size of the file system under `path` in bytes, or why it is not known: the system did not
/// answer, or it answered 0 (a file system that does not report its size, which a budget cannot
/// be compared with).
pub(super) fn size_of_file_system(disk: &dyn Disk, path: &Path) -> Result<u64, String> {
    match disk.space(path) {
        Ok(space) if space.total > 0 => Ok(space.total),
        Ok(_) => Err("the file system reports a size of 0 bytes".to_owned()),
        Err(why) => Err(format!(
            "the size of the file system could not be read: {why}"
        )),
    }
}

/// The line the start prints when the size of the file system is unknown and the budget is not
/// checked against it.
pub(super) fn unjudged_warning(why: &str, path: &Path) -> String {
    format!(
        "tierstore: warning: {why} (at {}); local_cache.budget is not checked against it",
        path.display()
    )
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
    // A file system whose size cannot be read is not judged: the start says so and goes on.
    match size_of_file_system(&*disk, cache.path()) {
        Ok(total) if config.budget > config.room(total) => {
            let text = format!(
                "local_cache.budget: {} bytes is more than the {} bytes of the file system at {} \
                 less its free_floor of {} percent",
                config.budget,
                total,
                config.path.display(),
                config.free_floor
            );
            return Err(Box::new(InvalidFlags(text)));
        }
        Ok(_) => {}
        Err(why) => eprintln!("{}", unjudged_warning(&why, cache.path())),
    }
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
    serve_cache_over(settings, upstream, None, disk, stop, ready).await
}

/// [`serve_cache`] over an upstream that may be a list of endpoints: `upstream` is read through
/// and `endpoints`, if given, is the same list as `upstream`, whose `/readyz` poll runs beside the
/// server and which the front shows on `/readyz` and `/metricz`.
///
/// # Errors
/// As [`serve_cache`].
pub(super) async fn serve_cache_over(
    settings: &CacheSettings,
    upstream: Arc<dyn Backend>,
    endpoints: Option<Arc<Endpoints>>,
    disk: Arc<dyn Disk>,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(SocketAddr),
) -> Result<Drained, Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut config = server_config(&settings.front, Arc::clone(&clock))?;
    let run = probe_run_id(&settings.front.proxy_id, clock.now(), std::process::id());
    let cache = open(&settings.cache, &upstream, &disk, &run).await?;
    reach(&*upstream, &run, &settings.upstream.spec).await?;
    if settings.cache.write_through {
        test_upstream(&*upstream, &run, &settings.upstream.spec).await?;
        config.cache_writes = true;
    }
    if let Some(rate) = settings.scrub_rate {
        eprintln!("tierstore: scrub_rate {rate} B/s is accepted; this release does not scrub");
    }
    config.cache = Some(cache.clone());
    config.upstream.clone_from(&endpoints);
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
    let polled = Arc::new(Notify::new());
    let unpolled = Arc::clone(&polled);
    let serving = async move {
        let drained = server.serve(listener).await;
        halting.notify_one();
        unpolled.notify_one();
        drained
    };
    let maintaining = cache.maintain(PASS_INTERVAL, halt);
    let polling = async {
        match &endpoints {
            Some(endpoints) => endpoints.watch(polled).await,
            None => drop(polled),
        }
    };
    let (drained, (), ()) = tokio::join!(serving, maintaining, polling);
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
    let open = |endpoint: Option<&str>| {
        let s3 = S3Flags {
            endpoint,
            region: tier.region.as_deref(),
            credentials: tier.credentials_file.as_deref(),
        };
        open_tier("--cold", &tier.spec, &s3, settings.s3_request_timeout)
    };
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    if settings.endpoints.is_empty() {
        let upstream = open(tier.endpoint.as_deref())?;
        return serve_cache(settings, upstream, disk, stop, ready).await;
    }
    let mut members = Vec::new();
    for url in &settings.endpoints {
        members.push((url.clone(), open(Some(url))?));
    }
    let probe = Arc::new(HttpProbe::new(READYZ_TIMEOUT));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let endpoints = Arc::new(Endpoints::new(members, settings.failover, clock, probe));
    let upstream: Arc<dyn Backend> = endpoints.clone();
    serve_cache_over(settings, upstream, Some(endpoints), disk, stop, ready).await
}

/// Runs the cache until SIGTERM or SIGINT.
pub(super) async fn serve_cache_until_signalled(settings: CacheSettings) -> Result<(), Error> {
    let stop = signals()?;
    let version = env!("CARGO_PKG_VERSION");
    let mode = if settings.cache.write_through {
        "write-through-cache"
    } else {
        "read-cache"
    };
    let announce = |addr: SocketAddr| println!("tierstore {version} listen={addr} mode={mode}");
    let drained = run_cache(&settings, stop, announce).await?;
    eprint!("{}", super::cli::cut_note(drained));
    Ok(())
}
