//! The counters of the OCI read front, shown by `GET /metricz` on the S3 listener.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// The calls the front tells apart in its counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RouteKind {
    Ping,
    Manifest,
    Blob,
    Other,
}

/// The label of each route, in the order of [`RouteKind`].
const ROUTES: [&str; 4] = ["ping", "manifest", "blob", "other"];

/// How a request ended, by the status it was answered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// 200.
    Ok,
    /// 206.
    Partial,
    /// 404.
    NotFound,
    /// 400.
    Invalid,
    /// 405.
    NotAllowed,
    /// 416.
    Unsatisfiable,
    /// 5xx: a store that failed, an object that is not what its name says.
    Failed,
}

/// The label of each outcome, in the order of [`Outcome`].
const OUTCOMES: [&str; 7] = [
    "ok",
    "partial",
    "not_found",
    "invalid",
    "not_allowed",
    "range_not_satisfiable",
    "failed",
];

/// What the front has done since it started: requests by route and outcome, the bytes of blobs
/// sent, and the objects whose bytes were not their digest.
#[derive(Debug, Default)]
pub struct OciMetrics {
    requests: [[AtomicU64; OUTCOMES.len()]; ROUTES.len()],
    bytes: AtomicU64,
    mismatches: AtomicU64,
}

impl OciMetrics {
    /// Counts one request.
    pub(super) fn request(&self, route: RouteKind, outcome: Outcome) {
        self.requests[route as usize][outcome as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts `count` bytes of a blob sent.
    pub(super) fn sent(&self, count: u64) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
    }

    /// Counts an object whose bytes were not the digest it is named by.
    pub(super) fn mismatch(&self) {
        self.mismatches.fetch_add(1, Ordering::Relaxed);
    }

    /// The requests counted for `route` and `outcome`.
    #[must_use]
    pub fn requests(&self, route: &str, outcome: &str) -> u64 {
        let route = ROUTES.iter().position(|name| *name == route);
        let outcome = OUTCOMES.iter().position(|name| *name == outcome);
        match (route, outcome) {
            (Some(route), Some(outcome)) => self.requests[route][outcome].load(Ordering::Relaxed),
            _ => 0,
        }
    }

    /// The bytes of blobs sent.
    #[must_use]
    pub fn bytes_sent(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The objects found not to be their digest.
    #[must_use]
    pub fn mismatches(&self) -> u64 {
        self.mismatches.load(Ordering::Relaxed)
    }

    /// The counters in the Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        text.push_str("# TYPE tierstore_oci_requests_total counter\n");
        for (r, route) in ROUTES.iter().enumerate() {
            for (o, outcome) in OUTCOMES.iter().enumerate() {
                let count = self.requests[r][o].load(Ordering::Relaxed);
                let labels = format!("route=\"{route}\",outcome=\"{outcome}\"");
                let _ = writeln!(text, "tierstore_oci_requests_total{{{labels}}} {count}");
            }
        }
        let bytes = self.bytes_sent();
        let _ = writeln!(text, "# TYPE tierstore_oci_bytes_served_total counter");
        let _ = writeln!(text, "tierstore_oci_bytes_served_total {bytes}");
        let mismatches = self.mismatches();
        let _ = writeln!(text, "# TYPE tierstore_oci_digest_mismatches_total counter");
        let _ = writeln!(text, "tierstore_oci_digest_mismatches_total {mismatches}");
        text
    }
}
