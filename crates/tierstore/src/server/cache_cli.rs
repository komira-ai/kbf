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
//! exit status 1, after the one try that the check of the start gives it. The probe keys that
//! earlier starts left are deleted after the start has reported itself ready, beside the server,
//! at most a hundred of them, and no new delete starts after ten seconds (see [`sweep_probes`]);
//! the rest waits for the next start.
//!
//! With an `[oci]` table the registry front runs beside the S3 one, over the same cache: a blob
//! or a manifest it reads is a hit on the disk or a fill from the upstream, as an S3 GET is. Its
//! flags and files are checked as a storage node's are (see `oci_setup_of`), before the cache is
//! opened, and it drains with the S3 front.
//!
//! On SIGTERM or SIGINT the process drains as a storage node does; the eviction pass is the only
//! thing running beside the server, and it stops with it. Nothing is owed to the upstream: no
//! journal, no lease, no copy in flight.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};

use crate::{
    Backend, Clock, Error as BackendError, MonotonicClock, SelfTestOptions, SweepBudget,
    SystemClock, self_test, sweep_probes,
};

use super::build_info::{COMMIT, VERSION};
use super::cli::{
    Addresses, Error, S3Flags, bind_oci, bind_s3, oci_cut_note, oci_setup_of, open_tier,
    probe_run_id, serve_oci, signals,
};
use super::config_cache::CacheSettings;
use super::front::{InvalidFlags, server_config};
use super::listen::{joined, local_addrs};
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

/// Deletes the probe keys that earlier starts left in the upstream, within the default budget (see
/// [`sweep_probes`]). It runs beside the server, after the start has reported itself ready: the
/// keys are in a prefix nothing reads, so a node need not wait for the deletes of a slow store.
async fn sweep_upstream(upstream: Arc<dyn Backend>, run: String, spec: String) {
    let name = format!("cold tier {spec}");
    let options = SelfTestOptions {
        name: &name,
        run_id: &run,
        multipart: true,
        skip: &[],
        wait: Duration::ZERO,
        pause: Duration::from_secs(1),
        max_pause: Duration::from_secs(30),
    };
    let note = |line: String| eprintln!("tierstore: {line}");
    let budget = SweepBudget::default();
    // The budget is a span of time, so it runs on a clock that the machine's clock being set
    // cannot move. The age of a key is judged on the store's clock, inside the sweep.
    sweep_probes(&*upstream, &options, budget, &MonotonicClock::new(), &note).await;
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
    ready: impl FnOnce(Addresses),
) -> Result<Drained, Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut config = server_config(&settings.front, Arc::clone(&clock))?;
    // The front's flags and files are checked before the cache is opened, as the storage node's.
    let oci = oci_setup_of(&settings.oci, &config, true)?;
    config.oci_metrics = oci.as_ref().map(|setup| Arc::clone(&setup.metrics));
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
    // The registry front reads what the S3 front reads: through the cache.
    let backend: Arc<dyn Backend> = Arc::new(cache.clone());
    let server = Server::new(Arc::clone(&backend), config);
    let listeners = bind_s3(&settings.front.listen)?;
    let oci = bind_oci(oci, backend).await?;
    ready(Addresses::of(local_addrs(&listeners)?, oci.as_ref())?);
    // After the ready: the sweep is the one thing of the self-test that is not a gate.
    let sweeping = settings.cache.write_through.then(|| {
        let spec = settings.upstream.spec.clone();
        tokio::spawn(sweep_upstream(Arc::clone(&upstream), run, spec))
    });
    let stopping = Arc::clone(&server);
    let (oci_stop, oci_stopped) = watch::channel(false);
    let lb_wait = settings.front.drain_lb_wait;
    let watcher = tokio::spawn(async move {
        stop.await;
        stopping.begin_drain();
        tokio::time::sleep(lb_wait).await;
        let _ = oci_stop.send(true);
    });
    let halt = Arc::new(Notify::new());
    let halting = Arc::clone(&halt);
    let serving = async move {
        let drained = server.serve_all(listeners).await;
        halting.notify_one();
        drained
    };
    let maintaining = cache.maintain(PASS_INTERVAL, halt);
    let (drained, (), oci_cut) = tokio::join!(serving, maintaining, serve_oci(oci, oci_stopped));
    watcher.abort();
    if let Some(sweeping) = sweeping {
        sweeping.abort();
    }
    eprint!("{}", oci_cut_note(oci_cut));
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
    ready: impl FnOnce(Addresses),
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
    let mode = if settings.cache.write_through {
        "write-through-cache"
    } else {
        "read-cache"
    };
    let announce = |bound: Addresses| {
        let listen = joined(&bound.s3_all);
        let mut line = format!("tierstore {VERSION} listen={listen} mode={mode}");
        if bound.oci.is_some() {
            let how = bound.oci_mode.unwrap_or_default().as_str();
            line.push_str(&format!(" oci-listen={} ({how})", joined(&bound.oci_all)));
        }
        println!("{line} commit={COMMIT}");
    };
    let drained = run_cache(&settings, stop, announce).await?;
    eprint!("{}", super::cli::cut_note(drained));
    Ok(())
}
