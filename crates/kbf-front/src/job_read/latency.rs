//! The latency of the job-facing reads: `kbf_job_read_seconds` by `op`, from a request's
//! arrival to its answer.
//!
//! A blob read and a batch are timed; `/locate`, a refused method and a path that names no
//! blob are not, so there is one observation per answered `GET /<hex>/<size>` and per
//! answered `POST /batch`. A blob streamed over the size where reads stream is answered when
//! its stream starts, not when its last byte is sent.

use std::future::Future;

use tonic::codegen::http::Method;

use super::{BATCH_PATH, digest_of};
use crate::metrics::{Families, Histogram, Unit};
use crate::store_metrics::{Stopwatch, wall_stopwatch};

/// The two reads that are timed: the label of the family.
#[derive(Clone, Copy)]
pub(super) enum ReadKind {
    /// `GET /<hex>/<size>`.
    Get,
    /// `POST /batch`.
    Batch,
}

impl ReadKind {
    const ALL: [Self; 2] = [Self::Get, Self::Batch];

    const fn label(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Batch => "batch",
        }
    }

    /// The kind of read `method` `path` is, if it is one that is timed.
    fn of(method: &Method, path: &str) -> Option<Self> {
        if path == BATCH_PATH {
            return (*method == Method::POST).then_some(Self::Batch);
        }
        (*method == Method::GET && digest_of(path).is_some()).then_some(Self::Get)
    }
}

/// What the timed reads took, and the clock that timed them.
pub(super) struct Latency {
    seconds: [Histogram; ReadKind::ALL.len()],
    clock: Stopwatch,
}

impl Latency {
    pub(super) fn new() -> Self {
        Self {
            seconds: std::array::from_fn(|_| Histogram::new(Unit::Seconds)),
            clock: wall_stopwatch(),
        }
    }

    pub(super) fn with_clock(mut self, clock: Stopwatch) -> Self {
        self.clock = clock;
        self
    }

    /// `answer`, observed if `method` `path` is a read that is timed.
    pub(super) async fn timed<T>(
        &self,
        (method, path): (&Method, &str),
        answer: impl Future<Output = T>,
    ) -> T {
        let kind = ReadKind::of(method, path);
        let began = (self.clock)();
        let reply = answer.await;
        if let Some(kind) = kind {
            let elapsed = (self.clock)().saturating_sub(began);
            self.seconds[kind as usize].observe_duration(elapsed);
        }
        reply
    }

    pub(super) fn render(&self, families: &mut Families) {
        let name = "kbf_job_read_seconds";
        families.head(
            name,
            "histogram",
            "Seconds from a job-facing blob read or batch (by op) to its answer, which for a \
             streamed blob is the start of the stream",
        );
        for kind in ReadKind::ALL {
            let labels = format!("op=\"{}\"", kind.label());
            self.seconds[kind as usize].render(&mut families.0, name, &labels);
        }
    }
}
