//! What a run measured: [`Tally`] while it runs, [`Report`] after, as a table and as
//! JSON.
//!
//! Latency percentiles are nearest-rank: p is the smallest time at or above which at
//! least p% of the requests' times lie (`ceil(p/100 * n)`-th smallest). A request's time
//! runs from its first poll, once it has an in-flight slot, to its answer (for a
//! ByteStream Write: the last message sent and the committed size answered).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde::Serialize;

use crate::bench::{Method, Options, Outcome};
use crate::tree::Tree;

/// The most error messages a report keeps; `failed` counts them all.
pub const MAX_ERRORS: usize = 10;

/// A kind of request, a row of the latency table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    FindMissing,
    /// ByteStream Write, one blob.
    Write,
    BatchUpdate,
    /// ByteStream Read, one blob.
    Read,
    BatchRead,
}

impl Kind {
    const fn name(self) -> &'static str {
        match self {
            Self::FindMissing => "find_missing",
            Self::Write => "write",
            Self::BatchUpdate => "batch_update",
            Self::Read => "read",
            Self::BatchRead => "batch_read",
        }
    }
}

/// The requests and blobs of a run so far.
#[derive(Debug, Default)]
pub struct Tally {
    pub(crate) times: BTreeMap<Kind, Vec<Duration>>,
    pub(crate) requests_planned: u64,
    pub(crate) requests_done: u64,
    pub(crate) blobs_planned: u64,
    pub(crate) blobs_done: u64,
    pub(crate) bytes_done: u64,
    pub(crate) failed: u64,
    pub(crate) errors: Vec<String>,
    pub(crate) missing: Vec<usize>,
    pub(crate) timed_out: bool,
}

impl Tally {
    pub(crate) fn add(&mut self, outcome: Outcome) {
        self.times
            .entry(outcome.kind)
            .or_default()
            .push(outcome.elapsed);
        self.requests_done += 1;
        self.blobs_done += outcome.blobs;
        self.bytes_done += outcome.bytes;
        self.failed += outcome.errors.len() as u64;
        let room = MAX_ERRORS - self.errors.len().min(MAX_ERRORS);
        self.errors.extend(outcome.errors.into_iter().take(room));
        self.missing.extend(outcome.missing);
    }

    /// One progress line.
    pub(crate) fn progress(&self, elapsed: Duration) -> String {
        format!(
            "{:.1} s: {}/{} requests, {}/{} blobs, {:.1} MB, {} failed",
            elapsed.as_secs_f64(),
            self.requests_done,
            self.requests_planned,
            self.blobs_done,
            self.blobs_planned,
            self.bytes_done as f64 / 1e6,
            self.failed
        )
    }
}

/// The time of one kind of request, in milliseconds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Latency {
    pub count: u64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

impl Latency {
    /// The nearest-rank percentiles of `times`, which must not be empty.
    #[must_use]
    pub fn of(mut times: Vec<Duration>) -> Self {
        times.sort_unstable();
        let n = times.len();
        let rank = |p: usize| times[(p * n).div_ceil(100).max(1) - 1].as_secs_f64() * 1e3;
        Self {
            count: n as u64,
            p50_ms: rank(50),
            p90_ms: rank(90),
            p99_ms: rank(99),
            max_ms: rank(100),
        }
    }
}

/// The tree a run moved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TreeSummary {
    pub files: u64,
    pub empty: u64,
    pub dirs: u64,
    pub file_bytes: u64,
    pub skipped: u64,
    /// Distinct blobs: the files' and the `Directory` messages (each digest once),
    /// and the `Tree` message.
    pub blobs: u64,
}

/// What one run did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    /// `upload` or `read`.
    pub command: &'static str,
    pub method: Method,
    pub in_flight: usize,
    pub find_missing: bool,
    pub batch_bytes: u64,
    /// The tree's seed.
    pub seed: u64,
    pub tree: TreeSummary,
    /// Blobs to send (after FindMissingBlobs) or to read.
    pub blobs_planned: u64,
    /// Blobs sent or read whole (and, read, checked against their digests).
    pub blobs_done: u64,
    pub bytes_done: u64,
    pub requests_planned: u64,
    pub requests_done: u64,
    /// From the first request to the last answer (or the deadline).
    pub wall_secs: f64,
    /// `blobs_done` over the wall time: what finished, so a run the deadline stopped
    /// reports the rate it reached, not the tree's size over the deadline.
    pub blobs_per_sec: f64,
    /// `bytes_done` in millions over the wall time.
    pub mb_per_sec: f64,
    /// Per kind of request.
    pub requests: BTreeMap<Kind, Latency>,
    /// Blobs or calls that failed.
    pub failed: u64,
    /// The first [`MAX_ERRORS`] failures.
    pub errors: Vec<String>,
    /// The deadline passed before every request finished.
    pub timed_out: bool,
}

impl Report {
    pub(crate) fn new(
        command: &'static str,
        tree: &Tree,
        options: Options,
        wall: Duration,
        tally: Tally,
    ) -> Self {
        let wall_secs = wall.as_secs_f64();
        Self {
            command,
            method: options.method,
            in_flight: options.in_flight,
            find_missing: options.find_missing,
            batch_bytes: options.batch_bytes,
            seed: tree.seed,
            tree: TreeSummary {
                files: tree.files,
                empty: tree.empty,
                dirs: tree.dirs,
                file_bytes: tree.file_bytes,
                skipped: tree.skipped,
                blobs: tree.blobs.len() as u64,
            },
            blobs_planned: tally.blobs_planned,
            blobs_done: tally.blobs_done,
            bytes_done: tally.bytes_done,
            requests_planned: tally.requests_planned,
            requests_done: tally.requests_done,
            wall_secs,
            blobs_per_sec: tally.blobs_done as f64 / wall_secs,
            mb_per_sec: tally.bytes_done as f64 / 1e6 / wall_secs,
            requests: tally
                .times
                .into_iter()
                .map(|(kind, times)| (kind, Latency::of(times)))
                .collect(),
            failed: tally.failed,
            errors: tally.errors,
            timed_out: tally.timed_out,
        }
    }

    /// Every planned blob moved, nothing failed, and the deadline did not pass.
    #[must_use]
    pub fn complete(&self) -> bool {
        (self.blobs_done == self.blobs_planned) & (self.failed == 0) & !self.timed_out
    }

    /// The report as a table for a terminal.
    #[must_use]
    pub fn table(&self) -> String {
        let t = &self.tree;
        let mut out = format!(
            "{} {}, {} in flight, find-missing {}, seed {}\n\
             tree: {} files ({} empty, {} skipped), {} dirs, {} bytes, {} blobs\n\
             {} of {} blobs, {} bytes, {} of {} requests in {:.3} s: {:.1} blobs/s, {:.2} MB/s\n\
             {:<14}{:>8}{:>11}{:>11}{:>11}{:>11}\n",
            self.command,
            self.method.name(),
            self.in_flight,
            if self.find_missing { "on" } else { "off" },
            self.seed,
            t.files,
            t.empty,
            t.skipped,
            t.dirs,
            t.file_bytes,
            t.blobs,
            self.blobs_done,
            self.blobs_planned,
            self.bytes_done,
            self.requests_done,
            self.requests_planned,
            self.wall_secs,
            self.blobs_per_sec,
            self.mb_per_sec,
            "request",
            "count",
            "p50 ms",
            "p90 ms",
            "p99 ms",
            "max ms",
        );
        for (kind, l) in &self.requests {
            let _ = writeln!(
                out,
                "{:<14}{:>8}{:>11.3}{:>11.3}{:>11.3}{:>11.3}",
                kind.name(),
                l.count,
                l.p50_ms,
                l.p90_ms,
                l.p99_ms,
                l.max_ms
            );
        }
        let _ = writeln!(
            out,
            "failed: {}{}",
            self.failed,
            if self.timed_out { ", timed out" } else { "" }
        );
        for e in &self.errors {
            let _ = writeln!(out, "  {e}");
        }
        out
    }
}
