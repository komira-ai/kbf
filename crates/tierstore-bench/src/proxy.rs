//! The fault proxy: a loopback HTTP/1.1 forwarder in front of the cold store (and, for the
//! clock-skew cell, the hot store) that adds the faults the cells need without touching the
//! store or a clock: added latency per request, a bandwidth cap, refused connections, a
//! blackhole, and, only when asked for, a shift of `Date` and `Last-Modified` in answers.
//!
//! What it forwards. Every request header goes through as received, `Host` included, so a
//! `SigV4` signature made for the proxy's address is checked by the store against the same
//! bytes (the one exception is the hop-by-hop set: `Connection`, `Keep-Alive`,
//! `Proxy-Connection`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`, which the two ends
//! frame for themselves). A request opens its own connection to the store, so the proxy adds
//! one connect per request; the cells run every proxied arm once in [`FaultMode::Pass`] as a
//! control, which is how that cost is known. Bodies are never buffered whole: each frame is
//! counted, paced when a bandwidth cap is on, and handed on before the next is read.
//!
//! Modes ([`FaultMode`]), changed at any time and read per request (a bandwidth cap per
//! frame, so a change reaches a transfer under way):
//! - `pass`: forward unchanged.
//! - `latency=MS` (also `delay=MS`): wait before forwarding each request.
//! - `bandwidth=BYTES`: at most that many bytes per second of a request body and of an answer
//!   body, per connection.
//! - `refuse`: a new connection is accepted and closed at once; a request on a connection
//!   that was already open fails.
//! - `blackhole`: the request is read and never answered.
//! - `skew=SECS`: forward, and move `Date` and `Last-Modified` of the answer by that many
//!   seconds. Header values only: a time inside a listing body is not touched. Refused
//!   unless the proxy was started with the rewrite allowed (`--allow-date-rewrite`).
//!
//! A fault expires: with a time to live (`--ttl-secs`, or `ttl=SECS` after the mode in the
//! control file) the proxy falls back to `pass` when it runs out, so a forgotten fault ends
//! by itself. `pass` never expires.
//!
//! Control. In process, [`ProxyHandle::set_mode`]. Across processes, a control file
//! (`--control FILE`): one line, `MODE [ttl=SECS]`, read every tick and applied when its
//! modification time or text changed since the last time it was read. Write it whole and
//! move it into place; a file that does not parse, or names a mode that is not allowed, is
//! counted in `control_rejected`, printed, and leaves the current mode alone.
//!
//! Counters. [`Counters`] (requests forwarded, bytes of request and answer bodies, connections
//! refused or held without an answer, mode changes, faults expired, control files rejected, counter writes that failed,
//! and the mode in force) are kept in memory ([`ProxyHandle::counters`],
//! [`ProxyHandle::request_count`]) and, with a result directory, written to
//! `proxy-counters.json` there every tick when they changed and when the proxy stops. The file
//! names the role (`proxy`) and no address.
//!
//! Time. The state of the mode is a value that is asked for the mode in force at a given
//! `Instant`; the unit tests of the expiry give it instants of their own and read no clock.
//! The verb `fault-proxy` is [`run`]: [`spawn`] and wait for SIGTERM.

use std::future::Future;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{DATE, HeaderMap, HeaderValue, LAST_MODIFIED};
use hyper::server::conn::http1 as server;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Sleep, sleep};

use crate::error::BenchError;
use crate::role::Role;

/// The counters file, in the result directory.
pub const COUNTERS_FILE: &str = "proxy-counters.json";

/// The version of [`Counters`] as written.
pub const COUNTERS_SCHEMA_VERSION: u32 = 1;

/// The headers each end of a connection frames for itself; never forwarded.
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// What the proxy does to a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultMode {
    /// Forward unchanged.
    Pass,
    /// Wait this many milliseconds before forwarding each request.
    Latency {
        /// Added milliseconds per request.
        ms: u64,
    },
    /// Limit the bytes per second of a request body and an answer body.
    Bandwidth {
        /// Bytes per second.
        bytes_per_sec: u64,
    },
    /// Refuse the connection.
    Refuse,
    /// Accept the connection and never answer.
    Blackhole,
    /// Shift the `Date` and `Last-Modified` of every answer by this many seconds.
    DateSkew {
        /// Seconds; negative is earlier.
        secs: i64,
    },
}

/// Why a mode was not understood.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("mode {0:?}: expected pass, refuse, blackhole, latency=MS, bandwidth=BYTES or skew=SECS")]
pub struct ModeError(String);

impl FromStr for FaultMode {
    type Err = ModeError;

    /// `pass`, `refuse`, `blackhole`, `latency=<ms>` (or `delay=<ms>`),
    /// `bandwidth=<bytes per second>`, `skew=<seconds>`.
    fn from_str(s: &str) -> Result<Self, ModeError> {
        let bad = || ModeError(s.to_owned());
        match s.split_once('=') {
            None => match s {
                "pass" => Ok(FaultMode::Pass),
                "refuse" => Ok(FaultMode::Refuse),
                "blackhole" => Ok(FaultMode::Blackhole),
                _ => Err(bad()),
            },
            Some(("latency" | "delay", n)) => Ok(FaultMode::Latency {
                ms: n.parse().map_err(|_| bad())?,
            }),
            Some(("bandwidth", n)) => Ok(FaultMode::Bandwidth {
                bytes_per_sec: n.parse().map_err(|_| bad())?,
            }),
            Some(("skew", n)) => Ok(FaultMode::DateSkew {
                secs: n.parse().map_err(|_| bad())?,
            }),
            Some(_) => Err(bad()),
        }
    }
}

impl std::fmt::Display for FaultMode {
    /// The text [`FromStr`] reads back.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FaultMode::Pass => f.write_str("pass"),
            FaultMode::Refuse => f.write_str("refuse"),
            FaultMode::Blackhole => f.write_str("blackhole"),
            FaultMode::Latency { ms } => write!(f, "latency={ms}"),
            FaultMode::Bandwidth { bytes_per_sec } => write!(f, "bandwidth={bytes_per_sec}"),
            FaultMode::DateSkew { secs } => write!(f, "skew={secs}"),
        }
    }
}

/// What to serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfig {
    /// The loopback address to listen on.
    pub listen: SocketAddr,
    /// The store's endpoint, `http://host:port`.
    pub upstream: String,
    /// The mode to start in.
    pub mode: FaultMode,
    /// How long a fault lasts before the proxy falls back to `pass`; none: until changed.
    pub ttl: Option<Duration>,
    /// The control file to read every tick.
    pub control: Option<PathBuf>,
    /// Where the counters file is written.
    pub result_dir: Option<PathBuf>,
    /// Whether `skew=SECS` may be set. Off by default.
    pub allow_date_rewrite: bool,
    /// How often the control file is read, an expired fault is settled and the counters are
    /// written.
    pub tick: Duration,
}

impl ProxyConfig {
    /// A proxy in `pass` mode with no expiry, no control file, no counters file, no date
    /// rewrite and a tick of 200 ms.
    #[must_use]
    pub fn new(listen: SocketAddr, upstream: impl Into<String>) -> Self {
        Self {
            listen,
            upstream: upstream.into(),
            mode: FaultMode::Pass,
            ttl: None,
            control: None,
            result_dir: None,
            allow_date_rewrite: false,
            tick: Duration::from_millis(200),
        }
    }
}

/// What the proxy has done so far, as kept in memory and as written to [`COUNTERS_FILE`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    /// [`COUNTERS_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// `proxy`.
    pub role: String,
    /// Requests sent on to the store, whatever came back.
    pub requests: u64,
    /// Bytes of request bodies read from the clients.
    pub request_bytes: u64,
    /// Bytes of answer bodies read from the store.
    pub response_bytes: u64,
    /// Connections and requests refused in `refuse` mode.
    pub refused: u64,
    /// Requests read and held without an answer in `blackhole` mode.
    pub blackholed: u64,
    /// Modes set, the first one at start included.
    pub mode_changes: u64,
    /// Faults that ran out their time to live.
    pub expired: u64,
    /// Control files read that were not applied.
    pub control_rejected: u64,
    /// Counter writes that failed (the next tick tries again).
    pub write_failures: u64,
    /// The mode in force, as [`FaultMode`] prints it.
    pub mode: String,
}

fn refusal(text: impl Into<String>) -> BenchError {
    BenchError::Proxy(text.into())
}

/// The error of a file or socket operation at `path`.
fn io_at(path: &Path) -> impl FnOnce(std::io::Error) -> BenchError + '_ {
    move |source| BenchError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// A mode the proxy will apply: a bandwidth of zero would never finish a request, and a date
/// rewrite must have been allowed at start.
fn check(mode: FaultMode, allow_date_rewrite: bool) -> Result<(), BenchError> {
    match mode {
        FaultMode::Bandwidth { bytes_per_sec: 0 } => {
            Err(refusal("bandwidth=0 would never finish a request"))
        }
        FaultMode::DateSkew { .. } if !allow_date_rewrite => Err(refusal(
            "skew=SECS rewrites Date and Last-Modified and is off unless --allow-date-rewrite",
        )),
        _ => Ok(()),
    }
}

/// `host:port` of `http://host:port[/]`.
fn upstream_authority(url: &str) -> Result<String, BenchError> {
    let valid = |authority: &&str| {
        let port_ok = authority
            .rsplit_once(':')
            .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
        port_ok && !authority.contains(['/', '@', '?', '#', ' '])
    };
    url.strip_prefix("http://")
        .map(|rest| rest.trim_end_matches('/'))
        .filter(valid)
        .map(str::to_owned)
        .ok_or_else(|| refusal(format!("upstream {url:?}: expected http://host:port")))
}

/// `MODE [ttl=SECS]`: the line of the control file.
fn parse_control(text: &str) -> Result<(FaultMode, Option<Duration>), String> {
    let mut words = text.split_whitespace();
    let mode = words
        .next()
        .unwrap_or("")
        .parse::<FaultMode>()
        .map_err(|e| e.to_string())?;
    let ttl = words
        .next()
        .map(|word| {
            word.strip_prefix("ttl=")
                .and_then(|n| n.parse::<NonZeroU64>().ok())
                .map(|n| Duration::from_secs(n.get()))
                .ok_or_else(|| format!("{word:?}: expected ttl=SECS, SECS above zero"))
        })
        .transpose()?;
    match words.next() {
        None => Ok((mode, ttl)),
        Some(extra) => Err(format!("{extra:?}: nothing may follow the time to live")),
    }
}

/// The mode in force, and until when.
#[derive(Clone, Debug)]
struct ModeState {
    mode: FaultMode,
    until: Option<Instant>,
    changes: u64,
    expired: u64,
}

impl ModeState {
    fn new() -> Self {
        Self {
            mode: FaultMode::Pass,
            until: None,
            changes: 0,
            expired: 0,
        }
    }

    /// The mode at `now`: `pass` once the time to live is up.
    fn effective(&self, now: Instant) -> FaultMode {
        match self.until {
            Some(until) if now >= until => FaultMode::Pass,
            _ => self.mode,
        }
    }

    /// Sets the mode; a fault (not `pass`) with a time to live ends `ttl` after `now`.
    fn set(&mut self, mode: FaultMode, ttl: Option<Duration>, now: Instant) {
        self.mode = mode;
        self.until = ttl
            .filter(|_| mode != FaultMode::Pass)
            .and_then(|ttl| now.checked_add(ttl));
        self.changes += 1;
    }

    /// Makes an expiry at `now` a fact of the state, once; true when it did.
    fn settle(&mut self, now: Instant) -> bool {
        match self.until {
            Some(until) if now >= until => {
                self.mode = FaultMode::Pass;
                self.until = None;
                self.expired += 1;
                true
            }
            _ => false,
        }
    }
}

/// The time `bytes` take at `bytes_per_sec`.
fn pace(bytes: usize, bytes_per_sec: u64) -> Duration {
    let nanos = u128::try_from(bytes).unwrap_or(u128::MAX) * 1_000_000_000
        / u128::from(bytes_per_sec.max(1));
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// The seconds since the epoch of the first instant `httpdate` cannot write (year 10000).
const END_OF_HTTP_DATES: u64 = 253_402_300_800;

/// `time` moved by `secs`, if it stays a time an HTTP date can name (1970 to 9999; the
/// writer panics outside it).
fn shift(time: SystemTime, secs: i64) -> Option<SystemTime> {
    let by = Duration::from_secs(secs.unsigned_abs());
    let moved = if secs >= 0 {
        time.checked_add(by)
    } else {
        time.checked_sub(by)
    };
    moved.filter(|moved| {
        moved
            .duration_since(UNIX_EPOCH)
            .is_ok_and(|since| since.as_secs() < END_OF_HTTP_DATES)
    })
}

/// Moves `Date` and `Last-Modified` by `secs`; a value that is not an HTTP date stays.
fn skew_headers(headers: &mut HeaderMap, secs: i64) {
    for name in [DATE, LAST_MODIFIED] {
        let moved = headers
            .get(&name)
            .and_then(|value| value.to_str().ok())
            .and_then(|text| httpdate::parse_http_date(text).ok())
            .and_then(|time| shift(time, secs))
            .and_then(|time| HeaderValue::try_from(httpdate::fmt_http_date(time)).ok());
        if let Some(value) = moved {
            headers.insert(name, value);
        }
    }
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

/// Which way a body goes.
#[derive(Clone, Copy, Debug)]
enum Side {
    Request,
    Response,
}

/// What every connection and the handle share.
struct Shared {
    mode: watch::Sender<ModeState>,
    upstream: String,
    allow_date_rewrite: bool,
    default_ttl: Option<Duration>,
    control: Option<PathBuf>,
    result_dir: Option<PathBuf>,
    requests: AtomicU64,
    request_bytes: AtomicU64,
    response_bytes: AtomicU64,
    refused: AtomicU64,
    blackholed: AtomicU64,
    control_rejected: AtomicU64,
    write_failures: AtomicU64,
}

impl Shared {
    fn new(upstream: String, config: &ProxyConfig) -> Self {
        Self {
            mode: watch::Sender::new(ModeState::new()),
            upstream,
            allow_date_rewrite: config.allow_date_rewrite,
            default_ttl: config.ttl,
            control: config.control.clone(),
            result_dir: config.result_dir.clone(),
            requests: AtomicU64::new(0),
            request_bytes: AtomicU64::new(0),
            response_bytes: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            blackholed: AtomicU64::new(0),
            control_rejected: AtomicU64::new(0),
            write_failures: AtomicU64::new(0),
        }
    }

    fn mode_now(&self) -> FaultMode {
        self.mode.borrow().effective(Instant::now())
    }

    /// Sets the mode at `now` after checking it; `ttl`, else the default one.
    fn apply_at(
        &self,
        mode: FaultMode,
        ttl: Option<Duration>,
        now: Instant,
    ) -> Result<(), BenchError> {
        check(mode, self.allow_date_rewrite)?;
        let ttl = ttl.or(self.default_ttl);
        self.mode.send_modify(|state| state.set(mode, ttl, now));
        Ok(())
    }

    fn bump(&self, counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }

    /// Counts the bytes of a frame on the side it went.
    fn count_bytes(&self, side: Side, bytes: usize) {
        let counter = match side {
            Side::Request => &self.request_bytes,
            Side::Response => &self.response_bytes,
        };
        self.bump(counter, u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    /// How long a frame of `bytes` is held under the mode in force.
    fn hold_for(&self, bytes: usize) -> Duration {
        match self.mode_now() {
            FaultMode::Bandwidth { bytes_per_sec } => pace(bytes, bytes_per_sec),
            _ => Duration::ZERO,
        }
    }

    fn counters(&self) -> Counters {
        let state = self.mode.borrow().clone();
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Counters {
            schema_version: COUNTERS_SCHEMA_VERSION,
            role: Role::Proxy.as_str().to_owned(),
            requests: load(&self.requests),
            request_bytes: load(&self.request_bytes),
            response_bytes: load(&self.response_bytes),
            refused: load(&self.refused),
            blackholed: load(&self.blackholed),
            mode_changes: state.changes,
            expired: state.expired,
            control_rejected: load(&self.control_rejected),
            write_failures: load(&self.write_failures),
            mode: state.effective(Instant::now()).to_string(),
        }
    }
}

/// A body that counts what passes and, under a bandwidth cap, holds each frame back for the
/// time its bytes take at the cap. It holds one frame, never the body.
struct Tap<B> {
    inner: B,
    side: Side,
    shared: Arc<Shared>,
    held: Option<Frame<Bytes>>,
    delay: Option<Pin<Box<Sleep>>>,
}

impl<B> Tap<B> {
    fn new(inner: B, side: Side, shared: Arc<Shared>) -> Self {
        Self {
            inner,
            side,
            shared,
            held: None,
            delay: None,
        }
    }
}

impl<B> Body for Tap<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = &mut *self;
        if let Some(delay) = this.delay.as_mut() {
            ready!(delay.as_mut().poll(cx));
            this.delay = None;
            return Poll::Ready(this.held.take().map(Ok));
        }
        let frame = match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
            None => return Poll::Ready(None),
            Some(Err(error)) => return Poll::Ready(Some(Err(error))),
            Some(Ok(frame)) => frame,
        };
        let bytes = frame.data_ref().map_or(0, Bytes::len);
        this.shared.count_bytes(this.side, bytes);
        let wait = this.shared.hold_for(bytes);
        if wait.is_zero() {
            return Poll::Ready(Some(Ok(frame)));
        }
        this.held = Some(frame);
        this.delay = Some(Box::pin(sleep(wait)));
        // Poll again at once: the sleep registers the waker.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }

    /// Over only when no frame is held back: the end of the inner body is not the end of this
    /// one while its last frame waits for the rate. (`&`, not `&&`: both are plain reads.)
    fn is_end_stream(&self) -> bool {
        self.held.is_none() & self.inner.is_end_stream()
    }
}

type Failure = Box<dyn std::error::Error + Send + Sync>;

/// One request: the fault of the mode in force, then the same request to the store.
async fn forward(
    shared: Arc<Shared>,
    request: Request<Incoming>,
) -> Result<Response<Tap<Incoming>>, Failure> {
    match shared.mode_now() {
        FaultMode::Refuse => {
            shared.bump(&shared.refused, 1);
            return Err("refused by the fault proxy".into());
        }
        FaultMode::Blackhole => {
            shared.bump(&shared.blackholed, 1);
            return std::future::pending().await;
        }
        FaultMode::Latency { ms } => sleep(Duration::from_millis(ms)).await,
        FaultMode::Pass | FaultMode::Bandwidth { .. } | FaultMode::DateSkew { .. } => {}
    }
    shared.bump(&shared.requests, 1);
    let upstream = TcpStream::connect(&shared.upstream).await?;
    upstream.set_nodelay(true).ok();
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(upstream)).await?;
    drop(tokio::spawn(connection));
    let (mut parts, body) = request.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    let outgoing = Request::from_parts(parts, Tap::new(body, Side::Request, Arc::clone(&shared)));
    let (mut head, body) = sender.send_request(outgoing).await?.into_parts();
    strip_hop_by_hop(&mut head.headers);
    if let FaultMode::DateSkew { secs } = shared.mode_now() {
        skew_headers(&mut head.headers, secs);
    }
    Ok(Response::from_parts(
        head,
        Tap::new(body, Side::Response, shared),
    ))
}

/// One client connection, until it ends.
async fn serve(shared: Arc<Shared>, stream: TcpStream) {
    stream.set_nodelay(true).ok();
    let service = service_fn(move |request| forward(Arc::clone(&shared), request));
    server::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
        .ok();
}

/// What the tick does: settles an expired fault, reads the control file, writes the counters.
struct Ticker {
    shared: Arc<Shared>,
    /// The modification time and text of the control file as last read.
    applied: Option<(Option<SystemTime>, String)>,
    /// The counters as last written.
    written: Option<Counters>,
}

impl Ticker {
    fn tick(&mut self, now: Instant) {
        self.shared.mode.send_if_modified(|state| state.settle(now));
        self.read_control(now);
        if self.flush().is_err() {
            self.shared.bump(&self.shared.write_failures, 1);
        }
    }

    fn read_control(&mut self, now: Instant) {
        let Some(path) = self.shared.control.as_deref() else {
            return;
        };
        let read = std::fs::read_to_string(path);
        if matches!(&read, Err(e) if e.kind() == ErrorKind::NotFound) {
            return;
        }
        let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let seen = (stamp, read.as_deref().unwrap_or("").to_owned());
        if self.applied.as_ref() == Some(&seen) {
            return;
        }
        self.applied = Some(seen);
        let outcome = read
            .map_err(|e| e.to_string())
            .and_then(|text| parse_control(&text))
            .and_then(|(mode, ttl)| {
                self.shared
                    .apply_at(mode, ttl, now)
                    .map_err(|e| e.to_string())
            });
        if let Err(reason) = outcome {
            self.shared.bump(&self.shared.control_rejected, 1);
            eprintln!("fault-proxy: control file not applied: {reason}");
        }
    }

    /// Writes the counters if they changed since the last write.
    fn flush(&mut self) -> Result<(), BenchError> {
        let Some(dir) = self.shared.result_dir.as_deref() else {
            return Ok(());
        };
        let counters = self.shared.counters();
        if self.written.as_ref() == Some(&counters) {
            return Ok(());
        }
        // Plain numbers and text: serialising them cannot fail.
        let json = serde_json::to_vec_pretty(&counters).unwrap_or_default();
        let target = dir.join(COUNTERS_FILE);
        let temp = dir.join(format!("{COUNTERS_FILE}.tmp"));
        std::fs::write(&temp, json).map_err(io_at(&temp))?;
        std::fs::rename(&temp, &target).map_err(io_at(&target))?;
        self.written = Some(counters);
        Ok(())
    }
}

/// Accepts connections and ticks until told to stop, then ends every connection.
async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    ticker: Arc<Mutex<Ticker>>,
    mut stop: watch::Receiver<bool>,
    tick: Duration,
) {
    let mut connections = JoinSet::new();
    let mut every = tokio::time::interval(tick);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = every.tick() => ticker.lock().await.tick(Instant::now()),
            accepted = listener.accept() => {
                // One finished connection is reaped per accept, which keeps the set as small as
                // the number of connections open.
                connections.try_join_next();
                // An accept that failed (descriptor limit) is skipped; the next one retries.
                accepted.into_iter().for_each(|(stream, _)| {
                    if shared.mode_now() == FaultMode::Refuse {
                        shared.bump(&shared.refused, 1);
                    } else {
                        connections.spawn(serve(Arc::clone(&shared), stream));
                    }
                });
            }
        }
    }
    connections.shutdown().await;
}

/// A running proxy. Dropping it ends it without the final counters write; [`Self::stop`]
/// writes them.
pub struct ProxyHandle {
    addr: SocketAddr,
    shared: Arc<Shared>,
    ticker: Arc<Mutex<Ticker>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl ProxyHandle {
    /// The address the proxy listens on (the port a `:0` listen address got).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Changes the fault for the requests from now on, expiring after the configured time to
    /// live.
    ///
    /// # Errors
    /// [`BenchError::Proxy`]: `bandwidth=0`, or `skew=SECS` without the date rewrite allowed.
    pub fn set_mode(&self, mode: FaultMode) -> Result<(), BenchError> {
        self.set_mode_for(mode, None)
    }

    /// [`Self::set_mode`] with a time to live of its own (`None`: the configured one).
    ///
    /// # Errors
    /// As [`Self::set_mode`].
    pub fn set_mode_for(&self, mode: FaultMode, ttl: Option<Duration>) -> Result<(), BenchError> {
        self.shared.apply_at(mode, ttl, Instant::now())
    }

    /// The mode in force now: `pass` once a fault has run out.
    #[must_use]
    pub fn mode(&self) -> FaultMode {
        self.shared.mode_now()
    }

    /// Requests sent on to the store so far, for the cells that count the cold reads an
    /// instance makes.
    #[must_use]
    pub fn request_count(&self) -> u64 {
        self.shared.requests.load(Ordering::Relaxed)
    }

    /// Everything counted so far.
    #[must_use]
    pub fn counters(&self) -> Counters {
        self.shared.counters()
    }

    /// Ends the proxy and every connection, writes the counters file (if there is a result
    /// directory) and returns the final counters.
    ///
    /// # Errors
    /// [`BenchError::Io`]: the counters file could not be written.
    pub async fn stop(mut self) -> Result<Counters, BenchError> {
        self.stop.send_replace(true);
        // A task that panicked has nothing more to say; the counters are in `shared`.
        (&mut self.task).await.ok();
        self.ticker.lock().await.flush()?;
        Ok(self.shared.counters())
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Binds and serves on the current runtime.
///
/// # Errors
/// [`BenchError::Proxy`]: the upstream is not `http://host:port`, or the first mode is not
/// allowed. [`BenchError::Io`]: the listen address could not be bound.
pub async fn spawn(config: ProxyConfig) -> Result<ProxyHandle, BenchError> {
    let upstream = upstream_authority(&config.upstream)?;
    let shared = Arc::new(Shared::new(upstream, &config));
    shared.apply_at(config.mode, None, Instant::now())?;
    let listen = config.listen.to_string();
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(io_at(Path::new(&listen)))?;
    let addr = listener.local_addr().map_err(io_at(Path::new(&listen)))?;
    let ticker = Arc::new(Mutex::new(Ticker {
        shared: Arc::clone(&shared),
        applied: None,
        written: None,
    }));
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(accept_loop(
        listener,
        Arc::clone(&shared),
        Arc::clone(&ticker),
        stopped,
        config.tick,
    ));
    Ok(ProxyHandle {
        addr,
        shared,
        ticker,
        stop,
        task,
    })
}

/// The verb: serves until SIGTERM, then writes the counters.
///
/// # Errors
/// As [`spawn`], and [`BenchError::Io`] when the signal handler or the final counters write
/// fails.
pub async fn run(config: ProxyConfig) -> Result<(), BenchError> {
    let mut terminate =
        signal(SignalKind::terminate()).map_err(io_at(Path::new("SIGTERM handler")))?;
    let handle = spawn(config).await?;
    eprintln!("fault-proxy: listening on {}", handle.local_addr());
    terminate.recv().await;
    handle.stop().await.map(|_| ())
}

#[cfg(test)]
mod tests;
