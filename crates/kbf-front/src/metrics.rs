//! The cache's upload-path metrics: counters, gauges and fixed-bucket histograms kept
//! in atomics, and rendered in the Prometheus text format ([`CacheMetrics::render`]),
//! which `kbf-server` serves as `GET /metrics` on its operator API listener.
//!
//! Every [`crate::Cache`] owns one [`CacheMetrics`], so the REAPI listener's services
//! and the worker listener's (daemons' uploads) count into the same families. Each
//! family's `# HELP` line says what it measures; `docs/api.md#get-metrics` says how to
//! read them together.
//!
//! Recording never waits and never fails: an observation is a few relaxed atomic adds.
//! A scrape reads the atomics one by one, so a family's `_sum` may lag its buckets by
//! an observation in flight; its `_count` is always the sum of its buckets.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kbf_meta::Command;

/// What a histogram's values are, which fixes its buckets and how they are printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// A latency, recorded in nanoseconds and printed in seconds.
    Seconds,
    /// A size in bytes.
    Bytes,
    /// A number of things (blobs in a segment).
    Count,
}

/// Latency bounds, in nanoseconds: 1 ms to 60 s.
const SECONDS_BOUNDS: [u64; 15] = [
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
    2_500_000_000,
    5_000_000_000,
    10_000_000_000,
    30_000_000_000,
    60_000_000_000,
];

/// Size bounds: 1 KiB to 1 GiB, by fours.
const BYTES_BOUNDS: [u64; 11] = [
    1 << 10,
    1 << 12,
    1 << 14,
    1 << 16,
    1 << 18,
    1 << 20,
    1 << 22,
    1 << 24,
    1 << 26,
    1 << 28,
    1 << 30,
];

/// Count bounds: 1 to 16384, by fours.
const COUNT_BOUNDS: [u64; 8] = [1, 4, 16, 64, 256, 1024, 4096, 16384];

impl Unit {
    const fn bounds(self) -> &'static [u64] {
        match self {
            Self::Seconds => &SECONDS_BOUNDS,
            Self::Bytes => &BYTES_BOUNDS,
            Self::Count => &COUNT_BOUNDS,
        }
    }

    /// A recorded value as Prometheus prints it.
    fn print(self, value: u64) -> String {
        match self {
            // Exact for every bound; a sum is printed to the nanosecond.
            Self::Seconds => format!("{}", value as f64 / 1e9),
            Self::Bytes | Self::Count => value.to_string(),
        }
    }
}

/// A count that only goes up.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// Adds `n`.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// The count now.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A level that goes up and down: requests in flight. Raised by [`Gauge::raise`],
/// lowered when the guard it returns is dropped, so an early return or a dropped
/// future lowers it too.
#[derive(Debug, Default)]
pub struct Gauge(AtomicU64);

impl Gauge {
    /// Raises the level by one until the guard is dropped.
    pub fn raise(&self) -> Raised<'_> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Raised(self)
    }

    /// The level now.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// One unit of a [`Gauge`], given back on drop.
#[derive(Debug)]
pub struct Raised<'a>(&'a Gauge);

impl Drop for Raised<'_> {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A histogram with fixed buckets for its [`Unit`].
#[derive(Debug)]
pub struct Histogram {
    unit: Unit,
    /// One count per bound, then one for values above the last bound (not cumulative).
    buckets: Box<[AtomicU64]>,
    sum: AtomicU64,
}

impl Histogram {
    /// An empty histogram of `unit`.
    #[must_use]
    pub fn new(unit: Unit) -> Self {
        let buckets = (0..=unit.bounds().len()).map(|_| AtomicU64::new(0));
        Self {
            unit,
            buckets: buckets.collect(),
            sum: AtomicU64::new(0),
        }
    }

    /// Records one value, in the unit's recorded form (nanoseconds for
    /// [`Unit::Seconds`]).
    pub fn observe(&self, value: u64) {
        let slot = self.unit.bounds().partition_point(|&b| b < value);
        self.buckets[slot].fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
    }

    /// Records a latency.
    pub fn observe_duration(&self, elapsed: Duration) {
        self.observe(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Records the time since `start`.
    pub fn since(&self, start: Instant) {
        self.observe_duration(start.elapsed());
    }

    /// How many values were recorded.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
    }

    /// The sum of the values recorded, in the recorded form.
    #[must_use]
    pub fn sum(&self) -> u64 {
        self.sum.load(Ordering::Relaxed)
    }

    /// The `_bucket`, `_sum` and `_count` lines of family `name`, each with `labels`
    /// (`key="value"` pairs, comma-separated, or empty) before `le`.
    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        let mut cumulative = 0;
        let bounds = self.unit.bounds().iter().map(|&b| self.unit.print(b));
        for (bucket, le) in self.buckets.iter().zip(bounds.chain(["+Inf".to_owned()])) {
            cumulative += bucket.load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels}{sep}le=\"{le}\"}} {cumulative}"
            );
        }
        let braced = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        let sum = self.unit.print(self.sum());
        let _ = writeln!(out, "{name}_sum{braced} {sum}");
        let _ = writeln!(out, "{name}_count{braced} {cumulative}");
    }
}

/// One kind of request: how many, how many failed, the bytes they carried, how many
/// are in flight, and how long each took from its arrival to its answer.
#[derive(Debug)]
pub struct Requests {
    /// Requests answered (or dropped by their caller).
    pub total: Counter,
    /// Requests that ended in an error, or were dropped before their answer.
    pub errors: Counter,
    /// Blob bytes the requests carried, counted as each message arrives (none for
    /// FindMissingBlobs).
    pub bytes: Counter,
    /// Requests begun and not yet answered.
    pub in_flight: Gauge,
    /// End-to-end latency, from the start of the handler to its answer.
    pub seconds: Histogram,
}

impl Default for Requests {
    fn default() -> Self {
        Self {
            total: Counter::default(),
            errors: Counter::default(),
            bytes: Counter::default(),
            in_flight: Gauge::default(),
            seconds: Histogram::new(Unit::Seconds),
        }
    }
}

impl Requests {
    /// Starts timing one request. It counts as an error unless [`Request::ok`] is
    /// called before the guard is dropped.
    pub fn start(&self) -> Request<'_> {
        Request {
            requests: self,
            started: Instant::now(),
            ok: false,
            _in_flight: self.in_flight.raise(),
        }
    }
}

/// One request of a [`Requests`], recorded when dropped.
#[derive(Debug)]
pub struct Request<'a> {
    requests: &'a Requests,
    started: Instant,
    ok: bool,
    _in_flight: Raised<'a>,
}

impl Request<'_> {
    /// Marks the request answered without error.
    pub const fn ok(&mut self) {
        self.ok = true;
    }

    /// Counts `n` bytes the request carried.
    pub fn bytes(&self, n: u64) {
        self.requests.bytes.add(n);
    }
}

impl Drop for Request<'_> {
    fn drop(&mut self) {
        self.requests.total.add(1);
        if !self.ok {
            self.requests.errors.add(1);
        }
        self.requests.seconds.since(self.started);
    }
}

/// The metadata commands the cache commits, as the `command` label names them.
pub const COMMANDS: [&str; 14] = [
    "tick",
    "alloc_epoch",
    "put_blob",
    "put_blobs",
    "put_action",
    "touch",
    "object_unreachable",
    "object_reachable",
    "collect",
    "add_tier",
    "put_staged",
    "seal_staged",
    "object_flushed",
    "flush_conflict",
];

/// The index into [`COMMANDS`] of `command`'s label.
#[must_use]
pub const fn command_label(command: &Command) -> usize {
    match command {
        Command::Tick(_) => 0,
        Command::AllocEpoch => 1,
        Command::PutBlob { .. } => 2,
        Command::PutBlobs(_) => 3,
        Command::PutAction { .. } => 4,
        Command::Touch(_) => 5,
        Command::ObjectUnreachable { .. } => 6,
        Command::ObjectReachable { .. } => 7,
        Command::Collect => 8,
        Command::AddTier { .. } => 9,
        Command::PutStaged { .. } => 10,
        Command::SealStaged { .. } => 11,
        Command::ObjectFlushed { .. } => 12,
        Command::FlushConflict { .. } => 13,
    }
}

/// Everything a [`crate::Cache`] measures. The fields are public so a test (or a
/// caller with its own exposition) can read them; [`CacheMetrics::render`] prints
/// them all.
#[derive(Debug)]
pub struct CacheMetrics {
    /// ByteStream Write calls, on every listener that serves this cache.
    pub write: Requests,
    /// ByteStream Writes answered at once because the blob was already durable.
    pub write_present: Counter,
    /// BatchUpdateBlobs calls.
    pub batch_update: Requests,
    /// Blobs sent in BatchUpdateBlobs calls.
    pub batch_update_blobs: Counter,
    /// FindMissingBlobs calls.
    pub find_missing: Requests,
    /// Digests asked about in FindMissingBlobs calls.
    pub find_missing_digests: Counter,
    /// Digests FindMissingBlobs answered missing.
    pub find_missing_missing: Counter,
    /// One store call (`Cache::store_each`, which ByteStream `Write`,
    /// `BatchUpdateBlobs` and `Cache::store_blobs` go through), whole: the query for
    /// held blobs, their touch, and the wait until every segment holding its blobs has
    /// been put, read back and committed (segments shared with other calls included).
    pub store_blobs: Histogram,
    /// One object PUT of a segment; a PUT tried again is observed once per attempt.
    pub segment_put: Histogram,
    /// One segment's footer read back from the store (two range GETs: the trailer,
    /// then the footer).
    pub segment_footer: Histogram,
    /// One segment's `PutBlobs` commit to the metadata log.
    pub segment_commit: Histogram,
    /// Blobs per segment written.
    pub segment_blobs: Histogram,
    /// Bytes per segment written, footer included.
    pub segment_bytes: Histogram,
    /// One range GET of a blob being read (ByteStream Read, GetActionResult, a
    /// BatchReadBlobs run).
    pub object_get: Histogram,
    /// Every metadata-log commit the cache makes, by command ([`COMMANDS`]).
    pub commit: [Histogram; COMMANDS.len()],
    /// Every metadata-log query the cache makes.
    pub query: Histogram,
}

impl Default for CacheMetrics {
    fn default() -> Self {
        Self {
            write: Requests::default(),
            write_present: Counter::default(),
            batch_update: Requests::default(),
            batch_update_blobs: Counter::default(),
            find_missing: Requests::default(),
            find_missing_digests: Counter::default(),
            find_missing_missing: Counter::default(),
            store_blobs: Histogram::new(Unit::Seconds),
            segment_put: Histogram::new(Unit::Seconds),
            segment_footer: Histogram::new(Unit::Seconds),
            segment_commit: Histogram::new(Unit::Seconds),
            segment_blobs: Histogram::new(Unit::Count),
            segment_bytes: Histogram::new(Unit::Bytes),
            object_get: Histogram::new(Unit::Seconds),
            commit: std::array::from_fn(|_| Histogram::new(Unit::Seconds)),
            query: Histogram::new(Unit::Seconds),
        }
    }
}

impl CacheMetrics {
    /// Every family in the Prometheus text exposition format (version 0.0.4). Each
    /// has a `# HELP` line saying what it measures; `docs/api.md#get-metrics` says how
    /// to read them together.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = Families(String::new());
        let write = &self.write;
        out.requests("kbf_bytestream_write", "ByteStream Write", write, true);
        out.counter(
            "kbf_bytestream_write_present_total",
            "ByteStream Writes answered at once: the blob was already durable",
            self.write_present.get(),
        );
        out.requests(
            "kbf_batch_update",
            "BatchUpdateBlobs",
            &self.batch_update,
            true,
        );
        out.counter(
            "kbf_batch_update_blobs_total",
            "Blobs sent in BatchUpdateBlobs calls",
            self.batch_update_blobs.get(),
        );
        out.requests(
            "kbf_find_missing",
            "FindMissingBlobs",
            &self.find_missing,
            false,
        );
        out.counter(
            "kbf_find_missing_digests_total",
            "Digests asked about in FindMissingBlobs calls",
            self.find_missing_digests.get(),
        );
        out.counter(
            "kbf_find_missing_missing_total",
            "Digests FindMissingBlobs answered missing",
            self.find_missing_missing.get(),
        );
        let histograms = [
            (
                "kbf_store_blobs_seconds",
                "One store of verified blobs: the held-blob query, its touch, and the wait \
                 for every segment holding its blobs (linger, PUT slot, PUT attempts and \
                 their backoff, footer read, and commit with the commits queued ahead)",
                &self.store_blobs,
            ),
            (
                "kbf_segment_put_seconds",
                "One PUT attempt of a segment's object; a retried PUT is observed per attempt",
                &self.segment_put,
            ),
            (
                "kbf_segment_footer_read_seconds",
                "One segment's footer read back after its PUT (two range GETs)",
                &self.segment_footer,
            ),
            (
                "kbf_segment_commit_seconds",
                "One segment's PutBlobs commit to the metadata log",
                &self.segment_commit,
            ),
            (
                "kbf_segment_blobs",
                "Blobs per segment written",
                &self.segment_blobs,
            ),
            (
                "kbf_segment_bytes",
                "Bytes per segment written, footer included",
                &self.segment_bytes,
            ),
            (
                "kbf_object_get_seconds",
                "One range GET of a blob being read",
                &self.object_get,
            ),
            (
                "kbf_meta_query_seconds",
                "Metadata-log queries the cache makes",
                &self.query,
            ),
        ];
        for (name, help, histogram) in histograms {
            out.head(name, "histogram", help);
            histogram.render(&mut out.0, name, "");
        }
        let name = "kbf_meta_commit_seconds";
        out.head(
            name,
            "histogram",
            "Metadata-log commits the cache makes, by command",
        );
        for (command, histogram) in COMMANDS.iter().zip(&self.commit) {
            histogram.render(&mut out.0, name, &format!("command=\"{command}\""));
        }
        out.0
    }
}

/// The text of a scrape, one family at a time.
struct Families(String);

impl Families {
    fn head(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}.\n# TYPE {name} {kind}");
    }

    fn counter(&mut self, name: &str, help: &str, value: u64) {
        self.head(name, "counter", help);
        let _ = writeln!(self.0, "{name} {value}");
    }

    fn gauge(&mut self, name: &str, help: &str, value: u64) {
        self.head(name, "gauge", help);
        let _ = writeln!(self.0, "{name} {value}");
    }

    /// The families of one kind of request, named `{prefix}_...`: requests, errors,
    /// bytes (unless `call` carries no blob bytes), in flight, and latency.
    fn requests(&mut self, prefix: &str, call: &str, requests: &Requests, bytes: bool) {
        let total = format!("{prefix}_requests_total");
        self.counter(
            &total,
            &format!("{call} calls answered or dropped"),
            requests.total.get(),
        );
        let errors = format!("{prefix}_errors_total");
        let help = format!("{call} calls that failed or were dropped before their answer");
        self.counter(&errors, &help, requests.errors.get());
        if bytes {
            let name = format!("{prefix}_bytes_total");
            let help = format!("Blob bytes {call} calls carried, counted as each message arrives");
            self.counter(&name, &help, requests.bytes.get());
        }
        let in_flight = format!("{prefix}_in_flight");
        let help = format!("{call} calls begun and not yet answered");
        self.gauge(&in_flight, &help, requests.in_flight.get());
        let seconds = format!("{prefix}_seconds");
        let help = format!("{call} latency, from the handler's start to its answer");
        self.head(&seconds, "histogram", &help);
        requests.seconds.render(&mut self.0, &seconds, "");
    }
}

#[cfg(test)]
mod tests;
