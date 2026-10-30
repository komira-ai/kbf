//! The start of a pass-through front: the S3 front over the cold store alone.
//!
//! The settings come from a file with `[cold_store]` and no other tier (see
//! [`config_cold`](super::config_cold)). The server's backend is the cold store itself, so every
//! call the front serves (reads, listings, create-only puts, multipart uploads, deletes,
//! presigned puts, and the calls of each key role) goes to it, with no journal, no lease and no
//! reconciler; `/usagez` says the usage is not watched, and `/metricz` that there is no local
//! cache.
//!
//! Create-only is a guarantee the clients rely on, and here only the cold store can keep it. The
//! S3 backend always claims the conditional put (it sends `If-None-Match: *` on a put and on the
//! completion of an upload, see [`S3Backend::new`](crate::S3Backend::new)), so the settings leave
//! no way to say the store does not (`skip_checks` is refused when the file is read, exit status
//! 2), and the start asks the cold store the whole self-test before the address is bound: a store
//! that overwrites when told not to, on a put or on a completion, is exit status 3.
//!
//! [`test_cold`] and [`open_cold`] are what a front over a cold store needs before it serves, apart
//! from this shape, so that another shape that writes through to the cold store starts the same
//! way.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;

use crate::{Backend, Clock, SelfTestOptions, SystemClock, self_test};

use super::cli::{
    Error, S3Flags, SELF_TEST_FIRST_PAUSE, SELF_TEST_MAX_PAUSE, open_tier, probe_run_id, signals,
};
use super::config_cold::ColdSettings;
use super::front::server_config;
use super::{Drained, Server};

/// The cold store the settings name, opened and not yet asked anything.
///
/// # Errors
/// A tier that cannot be made: a key file that cannot be read, an endpoint that is refused.
pub(super) fn open_cold(settings: &ColdSettings) -> Result<Arc<dyn Backend>, Error> {
    if settings.s3_request_timeout.is_zero() {
        return Err("--s3-request-timeout must be longer than zero".into());
    }
    let tier = &settings.cold;
    let s3 = S3Flags {
        endpoint: tier.endpoint.as_deref(),
        region: tier.region.as_deref(),
        credentials: tier.credentials_file.as_deref(),
    };
    open_tier("--cold", &tier.spec, &s3, settings.s3_request_timeout).map_err(Error::from)
}

/// Asks the cold store the questions of the self-test, those of its multipart uploads included,
/// and says on stderr how long it took. A store that does not answer is asked again for `wait`.
///
/// # Errors
/// The [`SelfTestError`](crate::SelfTestError) of a store that answers a check wrongly (exit
/// status 3) or does not answer within `wait` (exit status 1).
pub(super) async fn test_cold(
    cold: &dyn Backend,
    spec: &str,
    proxy_id: &str,
    wait: Duration,
) -> Result<(), Error> {
    let run_id = probe_run_id(proxy_id, SystemClock.now(), std::process::id());
    let name = format!("cold tier {spec}");
    let options = SelfTestOptions {
        name: &name,
        run_id: &run_id,
        multipart: true,
        skip: &[],
        wait,
        pause: SELF_TEST_FIRST_PAUSE,
        max_pause: SELF_TEST_MAX_PAUSE,
    };
    let note = |line: String| eprintln!("tierstore: {line}");
    eprintln!("tierstore: testing the {name}");
    let started = Instant::now();
    let outcome = self_test(cold, &options, &note).await;
    let took = started.elapsed().as_millis();
    eprintln!("tierstore: tested the {name} in {took} ms");
    outcome.map_err(Error::from)
}

/// Serves `cold` until `stop` resolves, and then drains.
///
/// # Errors
/// A cold store that fails the self-test, a key file that cannot be used, or an address that
/// cannot be bound.
pub(super) async fn serve_cold(
    settings: &ColdSettings,
    cold: Arc<dyn Backend>,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(SocketAddr),
) -> Result<Drained, Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let config = server_config(&settings.front, clock)?;
    let (spec, id) = (&settings.cold.spec, &settings.front.proxy_id);
    test_cold(&*cold, spec, id, settings.backend_check_wait).await?;
    let server = Server::new(cold, config);
    let address = settings.front.listen;
    let bound = TcpListener::bind(address).await;
    let listener = bound.map_err(|why| format!("binding {address}: {why}"))?;
    ready(listener.local_addr()?);
    let stopping = Arc::clone(&server);
    let watcher = tokio::spawn(async move {
        stop.await;
        stopping.begin_drain();
    });
    let drained = server.serve(listener).await;
    watcher.abort();
    Ok(drained)
}

/// Opens the cold store the settings name and serves it until `stop` resolves.
///
/// # Errors
/// As [`serve_cold`], and a cold store the settings do not make.
pub(super) async fn run_cold(
    settings: &ColdSettings,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(SocketAddr),
) -> Result<Drained, Error> {
    let cold = open_cold(settings)?;
    serve_cold(settings, cold, stop, ready).await
}

/// Runs the front until SIGTERM or SIGINT.
pub(super) async fn serve_cold_until_signalled(settings: ColdSettings) -> Result<(), Error> {
    let stop = signals()?;
    let version = env!("CARGO_PKG_VERSION");
    let announce =
        |addr: SocketAddr| println!("tierstore {version} listen={addr} mode=pass-through");
    let drained = run_cold(&settings, stop, announce).await?;
    eprint!("{}", super::cli::cut_note(drained));
    Ok(())
}
