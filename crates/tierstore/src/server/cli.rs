//! The flags and the start-up of the `tierstore` binary.
//!
//! One process runs three things over the same pair of tiers: a [`Proxy`] with the given id,
//! the S3 front of that proxy ([`Server`]), and a [`Reconciler`], which works only while it
//! holds the lease in the hot tier, so that any number of these processes may run over the same
//! tiers. Everything is a flag; the process reads no environment variable, and the secret
//! access key is read from a file, not from the command line.
//!
//! The tiers of this build are `memory` only: nothing is kept when the process stops, and the
//! objects of a memory tier carry the time zero. Tiers over S3 stores are added with the S3
//! backend of this crate, and then `--hot` and `--cold` name them.
//!
//! On SIGTERM or SIGINT the process drains (see the [`server`](super) docs): `/readyz` says 503,
//! it serves for `--drain-lb-wait`, refuses new requests, waits for those in flight and for the
//! answers it is sending for at most `--drain-deadline`, gives the reconciler's lease up and
//! exits with status 0. A start that fails exits with status 1, and flags that are not valid
//! with status 2.

use std::ffi::OsString;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::{
    Backend, Clock, MemoryBackend, Proxy, ProxyConfig, Reconciler, ReconcilerConfig, SystemClock,
};

use super::{Drained, Server, ServerConfig};

/// A start-up error, for the operator.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

const DEFAULT_MAX_REQUEST_BYTES: u64 = 256 << 20;

const DEFAULT_MAX_STAGED_BYTES: u64 = 1 << 30;

/// `tierstore`: an S3 server over a hot and a cold tier.
///
/// It serves one bucket with one access key, writes new objects to the hot tier, copies them
/// to the cold tier in the background, and frees the hot tier when it fills.
#[derive(Clone, Debug, Parser)]
#[command(name = "tierstore", version)]
pub struct Cli {
    /// The address the S3 API, `/healthz` and `/readyz` are served on.
    #[arg(long)]
    pub listen: SocketAddr,
    /// The id of this proxy, which names its journal in the hot tier and its lease. Keep it
    /// across restarts, and give each process over the same tiers one of its own.
    #[arg(long)]
    pub proxy_id: String,
    /// The hot tier: `memory`, the only kind this build opens. Its objects are lost when the
    /// process stops.
    #[arg(long)]
    pub hot: String,
    /// The cold tier: `memory`, the only kind this build opens.
    #[arg(long)]
    pub cold: String,
    /// The bytes the hot tier is meant to hold; the water marks are shares of it.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub hot_bytes: u64,
    /// The percent of `--hot-bytes` above which the oldest copied objects are evicted from the
    /// hot tier.
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub high_water: u8,
    /// The percent of `--hot-bytes` down to which the eviction goes on.
    #[arg(long, default_value_t = 70, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub low_water: u8,
    /// The percent of `--hot-bytes` above which a put waits a little first, and a read of a
    /// cold object does not copy it back into the hot tier.
    #[arg(long, default_value_t = 95, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub critical: u8,
    /// The name of the bucket served.
    #[arg(long)]
    pub bucket: String,
    /// The region in the credential scope of a request's signature.
    #[arg(long, default_value = "us-east-1")]
    pub region: String,
    /// The file that holds the access key: the access key id on the first line and the secret
    /// access key on the second. Only its owner may read it (mode 0600).
    #[arg(long)]
    pub access_key_file: PathBuf,
    /// How long, after the drain began, the server goes on serving everything, so that the
    /// 503 of `/readyz` is seen: a number and `ms`, `s`, `m` or `h`.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub drain_lb_wait: Duration,
    /// How long the drain waits for requests in flight, and for the answers being sent, once it
    /// refuses new requests.
    #[arg(long, default_value = "15m", value_parser = parse_duration)]
    pub drain_deadline: Duration,
    /// How long a batch of journal entries waits for more before it is written.
    #[arg(long, default_value = "20ms", value_parser = parse_duration)]
    pub journal_linger: Duration,
    /// How long an applied journal object is kept before it is deleted.
    #[arg(long, default_value = "24h", value_parser = parse_duration)]
    pub journal_retention: Duration,
    /// The most objects the reconciler copies to the cold tier at once.
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..))]
    pub copy_parallel: u32,
    /// How long the reconciler's lease lasts without a renewal.
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub lease_ttl: Duration,
    /// How often the reconciler works: shorter than `--lease-ttl`.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    pub reconcile_interval: Duration,
    /// The largest body of a request, in bytes: the largest object, and the largest part of a
    /// multipart upload.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_REQUEST_BYTES,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub max_request_bytes: u64,
    /// The most bytes that the parts of unfinished multipart uploads hold in memory.
    #[arg(long, default_value_t = DEFAULT_MAX_STAGED_BYTES)]
    pub max_staged_bytes: u64,
    /// How long a multipart upload that gets no part is kept.
    #[arg(long, default_value = "1h", value_parser = parse_duration)]
    pub upload_ttl: Duration,
}

/// A duration: a whole number of `ms`, `s`, `m` or `h`, such as `250ms` or `15m`.
pub(crate) fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || format!("{text:?} is not a duration; use a number and ms, s, m or h, as in 15m");
    let unit_at = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (digits, unit) = text.split_at(unit_at);
    let count: u64 = digits.parse().map_err(|_| bad())?;
    let millis = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err(bad()),
    };
    count
        .checked_mul(millis)
        .map(Duration::from_millis)
        .ok_or_else(bad)
}

/// The share of a budget that a percent is.
fn share(percent: u8) -> f64 {
    f64::from(percent) / 100.0
}

/// What the flags make.
struct Built {
    server: ServerConfig,
    proxy: ProxyConfig,
    reconciler: ReconcilerConfig,
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
}

/// The tier a flag names.
fn tier(flag: &str, spec: &str) -> Result<Arc<dyn Backend>, String> {
    match spec {
        "memory" => Ok(Arc::new(MemoryBackend::new())),
        _ => {
            let why = "only `memory` can be opened by this build; S3 tiers are not built in yet";
            Err(format!("{flag} {spec:?}: {why}"))
        }
    }
}

/// The access key id and the secret access key in the file.
fn read_key(path: &Path) -> Result<(String, String), String> {
    let shown = path.display();
    let fail = |why: String| format!("--access-key-file {shown}: {why}");
    let meta = std::fs::metadata(path);
    let meta = meta.map_err(|why| fail(why.to_string()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(fail("others can read it; make it mode 0600".to_owned()));
    }
    let text = std::fs::read_to_string(path);
    let text = text.map_err(|why| fail(why.to_string()))?;
    let mut lines = text.lines().map(str::trim);
    let id = lines.next().filter(|line| !line.is_empty());
    let secret = lines.next().filter(|line| !line.is_empty());
    match (id, secret) {
        (Some(id), Some(secret)) => Ok((id.to_owned(), secret.to_owned())),
        _ => Err(fail("the id is on the first line and the secret on the second".to_owned())),
    }
}

/// Checks the flags that depend on each other, and makes the settings they stand for.
fn build(cli: &Cli) -> Result<Built, String> {
    if cli.low_water >= cli.high_water {
        return Err("--low-water must be below --high-water".to_owned());
    }
    if cli.high_water > cli.critical {
        return Err("--high-water must not be above --critical".to_owned());
    }
    if cli.reconcile_interval >= cli.lease_ttl {
        return Err("--reconcile-interval must be shorter than --lease-ttl".to_owned());
    }
    if matches!(cli.bucket.as_str(), "healthz" | "readyz") {
        return Err(format!("--bucket {:?} is the name of a probe path", cli.bucket));
    }
    let (key_id, secret) = read_key(&cli.access_key_file)?;
    let hot = tier("--hot", &cli.hot)?;
    let cold = tier("--cold", &cli.cold)?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut server = ServerConfig::new(&cli.bucket, &key_id, &secret, Arc::clone(&clock));
    server.region = cli.region.clone();
    server.max_request_bytes = usize::try_from(cli.max_request_bytes).unwrap_or(usize::MAX);
    server.max_staged_bytes = cli.max_staged_bytes;
    server.upload_ttl = cli.upload_ttl;
    server.drain_lb_wait = cli.drain_lb_wait;
    server.drain_deadline = cli.drain_deadline;
    let mut proxy = ProxyConfig::new(&cli.proxy_id, cli.hot_bytes, Arc::clone(&clock));
    proxy.critical = share(cli.critical);
    proxy.linger = cli.journal_linger;
    let mut reconciler = ReconcilerConfig::new(&cli.proxy_id, cli.hot_bytes, clock);
    reconciler.high_water = share(cli.high_water);
    reconciler.low_water = share(cli.low_water);
    reconciler.parallel = cli.copy_parallel as usize;
    reconciler.lease_ttl = cli.lease_ttl;
    reconciler.retention = cli.journal_retention;
    Ok(Built {
        server,
        proxy,
        reconciler,
        hot,
        cold,
    })
}

/// How long [`reconcile`] waits for the lease to be given up once it is halted.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Runs a pass of `reconciler` now and then every `interval`, until `halt` is notified. Then
/// it gives the lease up. The calls that failed in a pass are reported on stderr.
///
/// A halt cuts the pass that is running, so that the process leaves within the drain deadline
/// whatever the pass is doing. Nothing goes on from a pass that is cut: the copies are
/// create-only and the checkpoints conditional, so the next reconciler does again what the pass
/// had not finished.
pub(crate) async fn reconcile(mut reconciler: Reconciler, interval: Duration, halt: Arc<Notify>) {
    loop {
        tokio::select! {
            biased;
            () = halt.notified() => break,
            report = reconciler.pass() => {
                for error in &report.failed {
                    eprintln!("tierstore: reconciler: {error}");
                }
            }
        }
        tokio::select! {
            () = halt.notified() => break,
            () = tokio::time::sleep(interval) => {}
        }
    }
    let _ = tokio::time::timeout(STOP_WAIT, reconciler.stop()).await;
}

/// Opens the proxy and serves it until `stop` resolves, and then drains.
///
/// `ready` is called with the address the server is bound to, before the first request is
/// taken. Returns when the drain is over.
///
/// # Errors
/// A flag that is not valid with the others, a key file or a tier that cannot be used, a proxy
/// that cannot be opened, or an address that cannot be bound.
pub async fn run(
    cli: &Cli,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(SocketAddr),
) -> Result<Drained, Error> {
    let built = build(cli)?;
    let (hot, cold) = (Arc::clone(&built.hot), Arc::clone(&built.cold));
    let opened = Proxy::open(hot, cold, built.proxy).await;
    let proxy = opened.map_err(|why| format!("opening the proxy: {why}"))?;
    let server = Server::new(Arc::new(proxy), built.server);
    let address = cli.listen;
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
    let reconciler = Reconciler::new(built.hot, built.cold, built.reconciler);
    let reconciling = reconcile(reconciler, cli.reconcile_interval, halt);
    let (drained, ()) = tokio::join!(serving, reconciling);
    watcher.abort();
    if drained.abandoned > 0 {
        let cut = drained.abandoned;
        eprintln!("tierstore: {cut} requests were cut at the drain deadline");
    }
    Ok(drained)
}

/// Resolves on SIGTERM or SIGINT. The handlers are in place when this returns.
fn signals() -> std::io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    })
}

async fn serve_until_signalled(cli: Cli) -> Result<(), Error> {
    let stop = signals()?;
    let version = env!("CARGO_PKG_VERSION");
    let announce = |addr: SocketAddr| println!("tierstore {version} listen={addr}");
    run(&cli, stop, announce).await?;
    Ok(())
}

fn blocking(cli: Cli) -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve_until_signalled(cli))
}

/// The binary: flags in, exit status out.
#[must_use]
pub fn main(args: Vec<OsString>) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(usage) => {
            let _ = usage.print();
            return ExitCode::from(if usage.use_stderr() { 2 } else { 0 });
        }
    };
    match blocking(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("tierstore: {why}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
