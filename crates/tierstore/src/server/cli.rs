//! The flags and the start-up of the `tierstore` binary.
//!
//! One process runs three things over the same pair of tiers: a [`Proxy`] with the given id,
//! the S3 front of that proxy ([`Server`]), and a [`Reconciler`], which works only while it
//! holds the lease in the hot tier, so that any number of these processes may run over the same
//! tiers. Everything is a flag; the process reads no environment variable, and the secret
//! access key is read from a file, not from the command line.
//!
//! The tiers are `memory`, `dir:<path>` and `s3:<bucket>`. A memory tier keeps nothing when the
//! process stops, and its objects carry the time zero. A directory tier keeps its objects, and
//! the multipart uploads in progress, in a directory of the local file system (see
//! [`DirBackend`](crate::DirBackend)), so that they outlive a restart; the directory is made if
//! it does not exist. A tier `s3:<bucket>` is a bucket of an S3-compatible store (see
//! [`S3Backend`]), which the flags of the tier say how to reach, with the name of the tier as
//! their prefix: `--hot-endpoint` or `--cold-endpoint` (`http://host[:port]`, required),
//! `--hot-region` or `--cold-region` (the signing region, `us-east-1` if not given), and
//! `--hot-credentials-file` or `--cold-credentials-file` (required: a file only its owner may
//! read, in the form of `--access-key-file` below; a secret on the command line would be
//! world-readable). The two tiers have a key of their own, so they may be two stores. These
//! flags are refused for a tier that is not an S3 bucket, since they would do nothing. A request
//! to an S3 tier is bounded by `--s3-request-timeout`. The cold tier must do multipart uploads,
//! which all three kinds do, and the proxy passes them to it.
//!
//! The clients of the S3 front sign with one of its access keys. `--access-key-file` is the full
//! key, which may do everything the front serves; each `--read-only-key-file` (the flag may be
//! given any number of times) is a key that may only read objects and list the bucket (see the
//! [`server`](super) docs), such as the key of the daemons that read blobs. The id of a key is
//! what a request names it by, so the ids must all differ. The file of every key and credential
//! flag holds the id and the secret either as JSON, `{"access_key_id": "...",
//! "secret_access_key": "..."}` (the form kbf's pack flags read; tierstore also refuses any other
//! field and an empty value), or as two lines, the id and then the secret; it is JSON when its
//! first byte that is not white space is `{`. Only its owner may read the file (mode 0600 or
//! 0400), and no message of the process names a secret.
//!
//! On SIGTERM or SIGINT the process drains (see the [`server`](super) docs): `/readyz` says 503,
//! it serves for `--drain-lb-wait`, refuses new requests, waits for those in flight and for the
//! answers it is sending for at most `--drain-deadline`, gives the reconciler's lease up and
//! exits with status 0. A start that fails exits with status 1. Flags that are not valid exit
//! with status 2: those the parser refuses, and two keys that have one id.
//!
//! Before it opens the proxy or binds the address, the process asks each tier the questions its
//! guarantees rest on (see [`self_test`]) under a probe key in `_tiering/selftest/` that is
//! new for each start and deleted afterwards. A tier that answers one wrongly (a create-only put
//! that overwrites, an `If-Match` that is ignored, a range answered with the whole object, a
//! head with no `Last-Modified`, a listing that ignores `start-after`, and on the cold tier a
//! multipart completion that overwrites) is not served over: the process says which tier, which
//! check and what the tier answered, and exits with status 3. A tier that cannot be reached (an
//! error that says it is down, 503, a timeout) is asked again, after pauses from one second doubling
//! to thirty, for `--backend-check-wait` (five minutes by default; each tier has all of it), and
//! then the start exits with status 1 and `tier unreachable: <tier>: <error>`. A tier that is full starts: the checks that
//! need a write are skipped, with a line that says so. `--skip-backend-check` leaves the
//! multipart-completion check out, for a reason that is logged at the start; no other check can
//! be left out.

use std::ffi::OsString;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::s3::{Capabilities, Credentials, S3Config};
use crate::{
    Backend, Check, Clock, DirBackend, MemoryBackend, Proxy, ProxyConfig, Reconciler,
    ReconcilerConfig, S3Backend, SelfTestError, SelfTestOptions, SystemClock, self_test,
};

use super::keys::{ensure_distinct, read_credentials, read_only_keys};
use super::{Drained, Server, ServerConfig};

/// A start-up error, for the operator.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Flags that cannot go together, which only the files they name show: the process exits with
/// status 2, as for flags the parser refused, since a restart cannot mend them.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct InvalidFlags(String);

const DEFAULT_MAX_REQUEST_BYTES: u64 = 256 << 20;

/// The streamed bodies taken at once: 32 of them hold at most 32 x (16 MiB + a few pieces).
const DEFAULT_MAX_CONCURRENT_PUTS: u32 = 32;

/// The least average rate of a body, in bytes a second.
const DEFAULT_PUT_MIN_BYTES_PER_SEC: u64 = 1024;

/// The signing region of an S3 tier whose region is not given.
const DEFAULT_REGION: &str = "us-east-1";

/// How long a connection to an S3 tier may take to be opened.
const S3_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The slowest rate, in bytes a second, at which the body of a write to an S3 tier is taken for
/// granted to go: a write may take its request timeout plus its length at this rate.
const S3_MIN_BODY_RATE: Option<NonZeroU64> = NonZeroU64::new(1 << 20);

/// `tierstore`: an S3 server over a hot and a cold tier.
///
/// It serves one bucket to the clients that sign with its full access key or with one of its
/// read-only keys, writes new objects to the hot tier, copies them to the cold tier in the
/// background, and frees the hot tier when it fills.
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
    /// The hot tier: `memory` (its objects are lost when the process stops), `dir:<path>` (a
    /// directory, made if it does not exist) or `s3:<bucket>` (a bucket of an S3-compatible
    /// store, which needs `--hot-endpoint` and `--hot-credentials-file`).
    #[arg(long)]
    pub hot: String,
    /// The cold tier: `memory`, `dir:<path>` or `s3:<bucket>` (which needs `--cold-endpoint`
    /// and `--cold-credentials-file`). Multipart uploads go straight to it, part by part, so it
    /// must do them; all three kinds do.
    #[arg(long)]
    pub cold: String,
    /// The endpoint of the hot tier when it is `s3:<bucket>`: `http://host[:port]`, with no
    /// path. TLS is not built yet.
    #[arg(long)]
    pub hot_endpoint: Option<String>,
    /// The region in the credential scope of the requests to the hot tier when it is
    /// `s3:<bucket>`; `us-east-1` if not given.
    #[arg(long)]
    pub hot_region: Option<String>,
    /// The file that holds the access key of the hot tier when it is `s3:<bucket>`: JSON,
    /// `{"access_key_id": "...", "secret_access_key": "..."}`, or the access key id on the first
    /// line and the secret access key on the second. Only its owner may read it (mode 0600).
    #[arg(long)]
    pub hot_credentials_file: Option<PathBuf>,
    /// The endpoint of the cold tier when it is `s3:<bucket>`: `http://host[:port]`, with no
    /// path. TLS is not built yet.
    #[arg(long)]
    pub cold_endpoint: Option<String>,
    /// The region in the credential scope of the requests to the cold tier when it is
    /// `s3:<bucket>`; `us-east-1` if not given.
    #[arg(long)]
    pub cold_region: Option<String>,
    /// The file that holds the access key of the cold tier when it is `s3:<bucket>`, in the
    /// form of `--hot-credentials-file`. Only its owner may read it (mode 0600).
    #[arg(long)]
    pub cold_credentials_file: Option<PathBuf>,
    /// The limit on a request to an `s3:<bucket>` tier: a number and `ms`, `s`, `m` or `h`. A
    /// request with a small body, or none, may take this long in all; a ranged read may take
    /// longer while bytes keep arriving; a write may take this long plus the time its body
    /// takes at 1 MiB a second; and the part of a multipart upload, which is as slow as the
    /// client that sends it, is cut only if it makes no progress for this long. The completion
    /// of a multipart upload has a small body, so it may take this long in all, and a store may
    /// take minutes to put a large object together: set this longer than the longest.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    pub s3_request_timeout: Duration,
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
    /// The file that holds the full access key, which may do everything the server serves:
    /// JSON, `{"access_key_id": "...", "secret_access_key": "..."}` (both fields and no other),
    /// or the access key id on the first line and the secret access key on the second. Only its
    /// owner may read it (mode 0600).
    #[arg(long)]
    pub access_key_file: PathBuf,
    /// A file that holds a read-only access key, in the form of `--access-key-file`: a key that
    /// may get and head objects, list the bucket and head the bucket, and is refused every
    /// write, delete and multipart call with 403 `AccessDenied`. Give the flag once for each
    /// such key (the daemons' key, say); the ids of all keys must differ, or the start is
    /// refused with status 2. Only its owner may read it (mode 0600).
    #[arg(long)]
    pub read_only_key_file: Vec<PathBuf>,
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
    /// The longest body of a request, in bytes: the longest object, and the longest part of a
    /// multipart upload. An upper bound on the length of what is streamed, not a bound on
    /// memory: the body of a put or of a part is passed on to the tiers as it arrives and is
    /// never held whole. A body over it is refused with 413 and nothing is stored; a request
    /// that says in its `Content-Length` that its body is longer is refused before any byte of
    /// it is read.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_REQUEST_BYTES,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub max_request_bytes: u64,
    /// The most put bodies and upload parts taken at once. One more is answered 503 `SlowDown`
    /// with `Retry-After: 1` and its body is not read. A body in flight holds a few pieces of
    /// at most the size the connection delivers (tens of KiB), and an S3 tier up to 16 MiB
    /// more, so 32 bodies hold about half a GiB at most, with S3 tiers, and a few MiB with
    /// directory tiers.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_CONCURRENT_PUTS,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub max_concurrent_puts: u32,
    /// The least average rate, in bytes a second, at which a request body may come in, counted
    /// from the start of the body once `--put-rate-grace` has passed. A slower body is
    /// answered 400 `RequestTimeout`, nothing of it is stored and the upload it began at an S3
    /// tier is aborted. It ends senders that send a byte inside every idle timeout. 0 turns
    /// it off.
    #[arg(long, default_value_t = DEFAULT_PUT_MIN_BYTES_PER_SEC)]
    pub put_min_bytes_per_sec: u64,
    /// How long a body is let to go before `--put-min-bytes-per-sec` is held to it: a number
    /// and `ms`, `s`, `m` or `h`.
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub put_rate_grace: Duration,
    /// How long a request body may go without a byte arriving, and how long a client may go
    /// without taking a byte of an answer, before the request ends: a number and `ms`, `s`,
    /// `m` or `h`. A body that arrives slowly but steadily is not cut by this timeout, however long
    /// the whole takes (`--put-min-bytes-per-sec` sets the slowest rate it is let to have). A request whose body stalls is answered 400 `RequestTimeout` and its connection
    /// is closed; a connection whose client stops reading an answer is closed. Both are
    /// logged on stderr, and a drain waits no longer than this for a stalled client.
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub body_idle_timeout: Duration,
    /// Leaves the multipart-completion check of the start-up self-test out, for a reason:
    /// `multipart-conditional=REASON`, where REASON says why the store cannot pass it (for
    /// example `multipart-conditional=store-ignores-if-none-match-on-completion`). It is the
    /// only check that can be left out: the others test the guarantees the proxy, the lease and
    /// the journal stand on, and any other name is refused as a usage error. The reason is
    /// written to stderr at every start. Without the check, a multipart upload may replace an
    /// object that exists: leave it out only for a store you have shown to be safe another way.
    /// None is skipped by default.
    #[arg(long, value_name = "CHECK=REASON", value_parser = parse_skip)]
    pub skip_backend_check: Vec<SkippedCheck>,
    /// How long the start-up self-test asks again of a tier that does not answer (down, a
    /// timeout, a 503 or another 5xx but 501 and 507), with pauses that double from one second
    /// to thirty, before the start fails with status 1 (and the unit starts it again): a number
    /// and `ms`, `s`, `m` or `h`. Each tier has the whole of it. `0s` asks once. A tier that
    /// answers wrongly is not asked again.
    #[arg(long, default_value = "5m", value_parser = parse_duration)]
    pub backend_check_wait: Duration,
}

/// A check of the self-test that the operator leaves out, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedCheck {
    /// The check left out.
    pub check: Check,
    /// The reason given, never empty.
    pub reason: String,
}

/// A skipped check: `multipart-conditional`, `=`, and a reason that is not empty. Any other
/// check is refused.
fn parse_skip(text: &str) -> Result<SkippedCheck, String> {
    let names: Vec<&str> = Check::ALL.iter().map(|check| check.name()).collect();
    let usage = format!(
        "{text:?} is not CHECK=REASON; the checks are {}",
        names.join(", ")
    );
    let (name, reason) = text.split_once('=').ok_or_else(|| usage.clone())?;
    let check = Check::parse(name).ok_or(usage)?;
    if check != Check::MultipartConditional {
        return Err(format!(
            "the {name} check cannot be skipped: only multipart-conditional can"
        ));
    }
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(format!(
            "the skipped check {name} needs a reason after the ="
        ));
    }
    Ok(SkippedCheck {
        check,
        reason: reason.to_owned(),
    })
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

/// The flags that say how to reach a tier that is an S3 bucket.
struct S3Flags<'a> {
    endpoint: Option<&'a str>,
    region: Option<&'a str>,
    credentials: Option<&'a Path>,
}

impl S3Flags<'_> {
    /// Whether any of the flags was given.
    fn given(&self) -> bool {
        let given = [
            self.endpoint.is_some(),
            self.region.is_some(),
            self.credentials.is_some(),
        ];
        given.contains(&true)
    }
}

impl Cli {
    /// The flags of the hot tier that are for an S3 bucket.
    fn hot_s3(&self) -> S3Flags<'_> {
        S3Flags {
            endpoint: self.hot_endpoint.as_deref(),
            region: self.hot_region.as_deref(),
            credentials: self.hot_credentials_file.as_deref(),
        }
    }

    /// The flags of the cold tier that are for an S3 bucket.
    fn cold_s3(&self) -> S3Flags<'_> {
        S3Flags {
            endpoint: self.cold_endpoint.as_deref(),
            region: self.cold_region.as_deref(),
            credentials: self.cold_credentials_file.as_deref(),
        }
    }
}

/// The tier a flag names, with the flags that go with it: a bucket of an S3-compatible store,
/// or one of the kinds that `tier` opens, which have no use for those flags.
fn open_tier(
    flag: &str,
    spec: &str,
    s3: &S3Flags<'_>,
    timeout: Duration,
) -> Result<Arc<dyn Backend>, String> {
    match spec.strip_prefix("s3:") {
        Some(bucket) => s3_tier(flag, spec, bucket, s3, timeout),
        None if s3.given() => {
            let why = "its endpoint, region and credentials flags are for an `s3:<bucket>` tier";
            Err(format!("{flag} {spec:?}: {why}"))
        }
        None => tier(flag, spec),
    }
}

/// The bucket `bucket` of the S3-compatible store that `s3` says how to reach, as the tier
/// that `flag` names with `spec`.
fn s3_tier(
    flag: &str,
    spec: &str,
    bucket: &str,
    s3: &S3Flags<'_>,
    timeout: Duration,
) -> Result<Arc<dyn Backend>, String> {
    let fail = |why: String| format!("{flag} {spec:?}: {why}");
    let Some(endpoint) = s3.endpoint else {
        return Err(fail(format!("it needs {flag}-endpoint")));
    };
    let Some(credentials) = s3.credentials else {
        return Err(fail(format!("it needs {flag}-credentials-file")));
    };
    let credentials_flag = format!("{flag}-credentials-file");
    let (key_id, secret) = read_credentials(&credentials_flag, credentials)?;
    let config = S3Config {
        endpoint: endpoint.to_owned(),
        region: s3.region.unwrap_or(DEFAULT_REGION).to_owned(),
        bucket: bucket.to_owned(),
        credentials: Credentials::new(key_id, secret),
        capabilities: Capabilities::default(),
        connect_timeout: S3_CONNECT_TIMEOUT,
        request_timeout: timeout,
        min_body_rate: S3_MIN_BODY_RATE,
    };
    let built = S3Backend::new(config);
    let backend = built.map_err(|why| fail(why.to_string()))?;
    Ok(Arc::new(backend))
}

/// The tier a flag names, of the kinds that need no flags besides the tier's own.
fn tier(flag: &str, spec: &str) -> Result<Arc<dyn Backend>, String> {
    match (spec, spec.strip_prefix("dir:")) {
        ("memory", _) => Ok(Arc::new(MemoryBackend::new())),
        (_, Some(path)) => match DirBackend::open(path) {
            Ok(dir) => Ok(Arc::new(dir)),
            Err(why) => Err(format!("{flag} {spec:?}: {why}")),
        },
        _ => {
            let why = "only `memory`, `dir:<path>` and `s3:<bucket>` are tiers";
            Err(format!("{flag} {spec:?}: {why}"))
        }
    }
}

/// The access key id and the secret access key in the file of `--access-key-file`.
fn read_key(path: &Path) -> Result<(String, String), String> {
    read_credentials("--access-key-file", path)
}

/// Checks the flags that depend on each other, and makes the settings they stand for.
fn build(cli: &Cli) -> Result<Built, Error> {
    if cli.low_water >= cli.high_water {
        return Err("--low-water must be below --high-water".into());
    }
    if cli.high_water > cli.critical {
        return Err("--high-water must not be above --critical".into());
    }
    if cli.reconcile_interval >= cli.lease_ttl {
        return Err("--reconcile-interval must be shorter than --lease-ttl".into());
    }
    if matches!(cli.bucket.as_str(), "healthz" | "readyz") {
        let name = &cli.bucket;
        return Err(format!("--bucket {name:?} is the name of a probe path").into());
    }
    if cli.body_idle_timeout.is_zero() {
        return Err("--body-idle-timeout must be longer than zero".into());
    }
    if cli.s3_request_timeout.is_zero() {
        return Err("--s3-request-timeout must be longer than zero".into());
    }
    let (key_id, secret) = read_key(&cli.access_key_file)?;
    let read_only = read_only_keys(&cli.read_only_key_file)?;
    ensure_distinct(&key_id, &read_only).map_err(InvalidFlags)?;
    let timeout = cli.s3_request_timeout;
    let hot = open_tier("--hot", &cli.hot, &cli.hot_s3(), timeout)?;
    let cold = open_tier("--cold", &cli.cold, &cli.cold_s3(), timeout)?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut server = ServerConfig::new(&cli.bucket, &key_id, &secret, Arc::clone(&clock));
    server.read_only_keys = read_only;
    server.region = cli.region.clone();
    server.max_request_bytes = usize::try_from(cli.max_request_bytes).unwrap_or(usize::MAX);
    server.body_idle_timeout = cli.body_idle_timeout;
    server.max_concurrent_puts = cli.max_concurrent_puts as usize;
    server.put_min_bytes_per_sec = cli.put_min_bytes_per_sec;
    server.put_rate_grace = cli.put_rate_grace;
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

/// The id of a start for the probe keys of its self-test: the proxy id (with any character a
/// key cannot carry replaced by `_`), the time of the start and the process id, so that two
/// starts never share a probe key.
fn probe_run_id(proxy_id: &str, now: u64, process: u32) -> String {
    let safe = |c: char| {
        if c.is_ascii_alphanumeric() || c == '-' {
            c
        } else {
            '_'
        }
    };
    let proxy: String = proxy_id.chars().map(safe).collect();
    format!("{proxy}-{now}-{process}")
}

/// The pause before the first repeat of a check of the self-test that could not reach its tier;
/// each repeat waits twice as long as the one before.
const SELF_TEST_FIRST_PAUSE: Duration = Duration::from_secs(1);

/// The longest pause between two attempts at a tier the self-test could not reach.
const SELF_TEST_MAX_PAUSE: Duration = Duration::from_secs(30);

/// Asks each tier the questions of the self-test, the hot tier first, and says on stderr which
/// checks the flags leave out, a line before and after each tier with how long it took, and
/// what the test notes (a check skipped because the tier is full, an attempt repeated, a probe key
/// not deleted). The cold tier is also asked about multipart uploads. A tier that did not answer
/// is asked again, after pauses from one second doubling to thirty, for `wait` (each tier has the
/// whole of it).
///
/// # Errors
/// The [`SelfTestError`] of the first tier to answer a check wrongly (a violation: exit status 3)
/// or not to answer within `wait` (unreachable: exit status 1).
async fn probe_tiers(
    cli: &Cli,
    hot: &dyn Backend,
    cold: &dyn Backend,
    wait: Duration,
) -> Result<(), Error> {
    let mut skip = Vec::new();
    for skipped in &cli.skip_backend_check {
        eprintln!(
            "tierstore: skipping the {} check of the tiers: {}",
            skipped.check, skipped.reason
        );
        skip.push(skipped.check);
    }
    let run_id = probe_run_id(&cli.proxy_id, SystemClock.now(), std::process::id());
    let hot_name = format!("hot tier {}", cli.hot);
    let cold_name = format!("cold tier {}", cli.cold);
    let tiers = [(&hot_name, hot, false), (&cold_name, cold, true)];
    for (name, backend, multipart) in tiers {
        let options = SelfTestOptions {
            name,
            run_id: &run_id,
            multipart,
            skip: &skip,
            wait,
            pause: SELF_TEST_FIRST_PAUSE,
            max_pause: SELF_TEST_MAX_PAUSE,
        };
        let note = |line: String| eprintln!("tierstore: {line}");
        eprintln!("tierstore: testing the {name}");
        let started = Instant::now();
        let outcome = self_test(backend, &options, &note).await;
        let took = started.elapsed().as_millis();
        eprintln!("tierstore: tested the {name} in {took} ms");
        outcome?;
    }
    Ok(())
}

/// Opens the proxy and serves it until `stop` resolves, and then drains.
///
/// The tiers are tested first (see [`self_test`]): a tier that fails is an error before the
/// proxy is opened or the address bound. `ready` is called with the address the server is bound
/// to, before the first request is taken. Returns when the drain is over.
///
/// # Errors
/// A flag that is not valid with the others, a key file or a tier that cannot be used, a tier
/// that fails the self-test (a [`SelfTestError`]), a proxy that cannot be opened, or an address
/// that cannot be bound.
pub async fn run(
    cli: &Cli,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(SocketAddr),
) -> Result<Drained, Error> {
    let built = build(cli)?;
    let (hot, cold) = (built.hot.as_ref(), built.cold.as_ref());
    probe_tiers(cli, hot, cold, cli.backend_check_wait).await?;
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
    eprint!("{}", cut_note(drained));
    Ok(drained)
}

/// What [`run`] reports on stderr of a drain: a line when the drain deadline cut requests, and
/// nothing when it cut none.
fn cut_note(drained: Drained) -> String {
    match drained.abandoned {
        0 => String::new(),
        cut => format!("tierstore: {cut} requests were cut at the drain deadline\n"),
    }
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
            ExitCode::from(exit_status(&why))
        }
    }
}

/// The exit status of a start that failed: 2 for flags that cannot go together, 3 for a tier
/// that answered the self-test wrongly, 1 for anything else (a tier that could not be reached
/// among it: a restart may mend that).
fn exit_status(why: &Error) -> u8 {
    if why.downcast_ref::<InvalidFlags>().is_some() {
        return 2;
    }
    match why.downcast_ref::<SelfTestError>() {
        Some(SelfTestError::Violation(_)) => 3,
        Some(SelfTestError::Unreachable { .. }) | None => 1,
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
