//! Requests to an object store, counted: [`Counted`] wraps a store and records each call
//! in a [`StoreMetrics`], which the scrape renders as families of its own.
//!
//! One family set per store the process talks to: the cold store (`kbf_cold_store_*`, read
//! with [`Cache::with_cold_store_metrics`](crate::Cache::with_cold_store_metrics)) and the
//! packs' (`kbf_pack_store_*`, [`Packs::with_store_metrics`](crate::Packs::with_store_metrics)).
//! Every family is labelled by [`Op`] alone (or, for bytes, by direction): never by key,
//! bucket or host, so the number of series is fixed. A family with no requests still renders,
//! with zeros.
//!
//! A request is counted when the call starts (the first poll of its future, so a future
//! dropped unpolled counts nothing) and timed, and its error counted, when the store
//! answers. A call that is dropped part way is a request without a latency. The bytes are
//! what the store returned or accepted, not what was asked for. `may_hold` and
//! `capabilities` are not requests (a store answers them without one) and are passed on
//! uncounted.
//!
//! Recording is a few relaxed atomic adds and takes no lock.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

use crate::metrics::{Counter, Histogram, Unit};

/// A monotonic reading: the time since some fixed point. [`Counted`] reads it twice around
/// a call, so a test hands it a clock that it moves itself.
pub type Stopwatch = Arc<dyn Fn() -> Duration + Send + Sync>;

/// The stopwatch of a process: time since it was made.
#[must_use]
pub fn wall_stopwatch() -> Stopwatch {
    let origin = Instant::now();
    Arc::new(move || origin.elapsed())
}

/// The kind of request a store is asked, the one label of its families.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// A ranged read (`get_range`).
    Get,
    /// A length lookup (`len`), a HEAD for a store over the network.
    Head,
    /// A write (`put_new`, `put_stream`).
    Put,
    /// A delete.
    Delete,
    /// One page of a listing.
    List,
}

impl Op {
    /// Every op, in the order the families list them.
    pub const ALL: [Self; 5] = [Self::Get, Self::Head, Self::Put, Self::Delete, Self::List];

    /// The label value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Head => "head",
            Self::Put => "put",
            Self::Delete => "delete",
            Self::List => "list",
        }
    }
}

/// Which store the families are of: it fixes their names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Store {
    Cold,
    Packs,
}

/// What a counted store has done: requests and errors by [`Op`], a latency histogram by
/// [`Op`], and the bytes read and written.
#[derive(Debug)]
pub struct StoreMetrics {
    store: Store,
    requests: [Counter; Op::ALL.len()],
    errors: [Counter; Op::ALL.len()],
    seconds: [Histogram; Op::ALL.len()],
    read_bytes: Counter,
    write_bytes: Counter,
}

impl StoreMetrics {
    fn new(store: Store) -> Self {
        Self {
            store,
            requests: Default::default(),
            errors: Default::default(),
            seconds: std::array::from_fn(|_| Histogram::new(Unit::Seconds)),
            read_bytes: Counter::default(),
            write_bytes: Counter::default(),
        }
    }

    /// The cold store's: `kbf_cold_store_*`.
    #[must_use]
    pub fn cold() -> Self {
        Self::new(Store::Cold)
    }

    /// The packs' store's: `kbf_pack_store_*`.
    #[must_use]
    pub fn packs() -> Self {
        Self::new(Store::Packs)
    }

    /// The requests of `op` begun.
    #[must_use]
    pub fn requests(&self, op: Op) -> u64 {
        self.requests[op as usize].get()
    }

    /// The requests of `op` that the store answered with an error.
    #[must_use]
    pub fn errors(&self, op: Op) -> u64 {
        self.errors[op as usize].get()
    }

    /// The latency of the requests of `op` that the store answered.
    #[must_use]
    pub fn seconds(&self, op: Op) -> &Histogram {
        &self.seconds[op as usize]
    }

    /// The bytes the store returned.
    #[must_use]
    pub fn read_bytes(&self) -> u64 {
        self.read_bytes.get()
    }

    /// The bytes the store accepted.
    #[must_use]
    pub fn write_bytes(&self) -> u64 {
        self.write_bytes.get()
    }

    /// Appends the families to `out`: requests, errors and request seconds by `op`, and
    /// bytes by `dir`.
    pub fn render(&self, out: &mut String) {
        let (name, what) = match self.store {
            Store::Cold => ("kbf_cold_store", "the cold object store"),
            Store::Packs => ("kbf_pack_store", "the packs' object store"),
        };
        let by_op = |out: &mut String, suffix: &str, kind: &str, help: String| {
            let _ = writeln!(out, "# HELP {name}_{suffix} {help}");
            let _ = writeln!(out, "# TYPE {name}_{suffix} {kind}");
        };
        by_op(
            out,
            "requests_total",
            "counter",
            format!("Requests made to {what}, by op, counted when the call starts"),
        );
        for op in Op::ALL {
            let _ = writeln!(
                out,
                "{name}_requests_total{{op=\"{}\"}} {}",
                op.label(),
                self.requests(op)
            );
        }
        by_op(
            out,
            "errors_total",
            "counter",
            format!("Requests to {what} the store answered with an error, by op"),
        );
        for op in Op::ALL {
            let _ = writeln!(
                out,
                "{name}_errors_total{{op=\"{}\"}} {}",
                op.label(),
                self.errors(op)
            );
        }
        by_op(
            out,
            "bytes_total",
            "counter",
            format!(
                "Bytes read from and written to {what}, as the store returned or accepted them"
            ),
        );
        let _ = writeln!(
            out,
            "{name}_bytes_total{{dir=\"read\"}} {}",
            self.read_bytes()
        );
        let _ = writeln!(
            out,
            "{name}_bytes_total{{dir=\"write\"}} {}",
            self.write_bytes()
        );
        by_op(
            out,
            "request_seconds",
            "histogram",
            format!("Seconds from a request to {what} to its answer, by op"),
        );
        for op in Op::ALL {
            let labels = format!("op=\"{}\"", op.label());
            self.seconds(op)
                .render(out, &format!("{name}_request_seconds"), &labels);
        }
    }
}

/// One request in progress: counted when begun, timed and judged when ended.
struct Call<'a> {
    metrics: &'a StoreMetrics,
    op: Op,
    clock: &'a Stopwatch,
    began: Duration,
}

impl<'a> Call<'a> {
    fn begin(metrics: &'a StoreMetrics, clock: &'a Stopwatch, op: Op) -> Self {
        metrics.requests[op as usize].add(1);
        Self {
            metrics,
            op,
            clock,
            began: clock(),
        }
    }

    /// Records the answer and gives it back unchanged.
    fn end<T>(self, answer: Result<T, ObjectStoreError>) -> Result<T, ObjectStoreError> {
        let elapsed = (self.clock)().saturating_sub(self.began);
        self.metrics.seconds[self.op as usize].observe_duration(elapsed);
        if answer.is_err() {
            self.metrics.errors[self.op as usize].add(1);
        }
        answer
    }
}

/// `S`, with every request it is asked recorded in a [`StoreMetrics`]. Answers and errors
/// pass through unchanged.
pub struct Counted<S> {
    inner: S,
    metrics: Arc<StoreMetrics>,
    clock: Stopwatch,
}

impl<S> Counted<S> {
    /// `inner`, counted in `metrics`, timed by the process's clock.
    #[must_use]
    pub fn new(inner: S, metrics: Arc<StoreMetrics>) -> Self {
        Self::with_stopwatch(inner, metrics, wall_stopwatch())
    }

    /// [`Counted::new`] timed by `clock`.
    #[must_use]
    pub fn with_stopwatch(inner: S, metrics: Arc<StoreMetrics>, clock: Stopwatch) -> Self {
        Self {
            inner,
            metrics,
            clock,
        }
    }

    fn call(&self, op: Op) -> Call<'_> {
        Call::begin(&self.metrics, &self.clock, op)
    }
}

impl<S> std::fmt::Debug for Counted<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Counted").finish_non_exhaustive()
    }
}

impl<S: ObjectStore> ObjectStore for Counted<S> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_stream(key, len, body).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_range(key, range).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        self.inner.may_hold(key).await
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        self.inner.len(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}
