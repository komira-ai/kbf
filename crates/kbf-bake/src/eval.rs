//! The evaluator: one scrape in, the decision out. It reads no clock and does no I/O; the
//! time of a scrape is the scrape's own field, so a test pins it.

use kbf_gate::{Floor, GateResult, Limit, Metrics, MinLoad, Sample, Status, Verdict};

use crate::state::{BucketCount, Counter, State};
use crate::table::{
    ERROR_ADD_POINTS, ERROR_BASELINE_FACTOR, LATENCY_RED_FACTOR, LATENCY_WARN_FACTOR, MIN_ACTIONS,
    MIN_BUILDS, MIN_HOURS, UNDER_REPLICATED_SECS, is_farm_class, metric,
};
use crate::updater::UpdaterLog;

/// One scrape of the farm.
pub struct Observation<'a> {
    /// When it was taken (unix seconds).
    pub at: u64,
    /// The `/metrics` text, read.
    pub metrics: &'a Metrics,
    /// Builds seen since the bake began, counted from the invocation records; `None` when
    /// they could not be counted, which counts as none.
    pub builds: Option<u64>,
    /// The updater's phase log; `None` when it could not be read, which fails the restarts gate.
    pub updater: Option<&'a UpdaterLog>,
}

/// How the bake ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    /// Green, red, or extended (no decision).
    pub status: Status,
    /// Why, in a sentence.
    pub note: String,
}

/// What a scrape decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Every gate, as this scrape judged it.
    pub gates: Vec<GateResult>,
    /// The minimum-load numbers so far.
    pub load: MinLoad,
    /// Set when the bake is over (or extended).
    pub end: Option<End>,
}

/// When a gate may end the bake.
#[derive(Clone, Copy, PartialEq, Eq)]
enum When {
    /// Every scrape.
    Always,
    /// Once the minimum number of actions has run (a ratio over a handful of calls decides
    /// nothing), and at the end.
    AfterFloor,
    /// Only at the end of the window.
    AtEnd,
}

/// The bake evaluator over a [`State`].
pub struct Evaluator {
    state: State,
    max_gap: u64,
}

impl Evaluator {
    /// An evaluator over `state`. A gap between two scrapes longer than `max_gap` seconds is
    /// time nothing watched, and the bake extends by it.
    pub fn new(state: State, max_gap: u64) -> Self {
        Self { state, max_gap }
    }

    /// The state, to be saved after each scrape.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Record a waiver (the same `what` is kept once).
    pub fn waive(&mut self, w: kbf_gate::Waiver) {
        if !self.state.waivers.iter().any(|x| x.what == w.what) {
            self.state.waivers.push(w);
        }
    }

    /// Account for time nobody watched ending at `at`: the bake extends by it.
    fn note_gap(&mut self, at: u64) {
        let since = self.state.last_scrape.unwrap_or(self.state.bake_start);
        let gap = at.saturating_sub(since);
        if gap > self.max_gap {
            self.state.blind_secs += gap;
        }
        self.state.last_scrape = Some(at);
    }

    /// Seconds of the bake that were watched.
    fn watched(&self, at: u64) -> u64 {
        at.saturating_sub(self.state.bake_start)
            .saturating_sub(self.state.blind_secs)
    }

    /// Take one scrape, update the state and decide.
    pub fn observe(&mut self, o: &Observation<'_>) -> Decision {
        self.note_gap(o.at);
        let c = &mut self.state.counters;
        let m = o.metrics;
        let failed = Counter::follow(
            &mut c.failed,
            m.sum_by(metric::FAILED_BY_CLASS, metric::CLASS_LABEL, &is_farm_class),
        );
        let losses = Counter::follow(&mut c.losses, m.sum(metric::BLOB_LOSSES));
        let errors = Counter::follow(&mut c.errors, m.sum(metric::EXECUTE_ERRORS));
        let before = c.requests.map_or(0.0, |r| r.increase);
        let requests = Counter::follow(&mut c.requests, m.sum(metric::EXECUTE_REQUESTS));
        let flush = Counter::follow(&mut c.flush_failures, m.sum(metric::FLUSH_FAILURES));
        let digest = Counter::follow(&mut c.digest_mismatches, m.sum(metric::DIGEST_MISMATCHES));
        let queue_seen = follow_histogram(&mut c.queued, m);
        if requests.is_some_and(|r| r > before) {
            self.state.load_hours.insert(o.at / 3600);
        }
        self.state.builds = o.builds.unwrap_or(self.state.builds);

        let under = m.sum(metric::UNDER_REPLICATED);
        self.follow_under_replicated(under, o);
        let stretch = self.state.under_since.map_or(0, |s| o.at.saturating_sub(s));

        let restarts = o
            .updater
            .map(|l| l.restarts_in(self.state.bake_start, o.at));
        let ratio = error_ratio(errors, requests);
        let (error_sample, error_limit) = self.error_gate(ratio);
        let p99_ratio = self.latency_ratio().filter(|_| queue_seen);
        let started = self.state.bake_start;
        let g = |name: &str, s: Sample, l: Limit, when| {
            (GateResult::decide(name, s, l, started, o.at), when)
        };
        let count = |v: Option<f64>| v.map_or(Sample::Absent, Sample::Value);
        let zero = Limit::AtMost(0.0);
        let gates = vec![
            g("farm-class-failures", count(failed), zero, When::Always),
            g("lost-blobs", m.sum(metric::LOST_BLOBS), zero, When::Always),
            g("blob-losses-delta", count(losses), zero, When::Always),
            g("under-replicated", under, zero, When::AtEnd),
            g(
                "under-replicated-secs",
                Sample::Value(stretch as f64),
                Limit::AtMost(UNDER_REPLICATED_SECS as f64),
                When::Always,
            ),
            g(
                "relay-untraced",
                m.sum(metric::RELAY_UNTRACED),
                zero,
                When::Always,
            ),
            g("error-rate", error_sample, error_limit, When::AfterFloor),
            g(
                "latency-p99-ratio",
                count(p99_ratio),
                Limit::Band {
                    warn: LATENCY_WARN_FACTOR,
                    red: LATENCY_RED_FACTOR,
                },
                When::AfterFloor,
            ),
            g(
                "write-tier-flush-failures",
                count(flush),
                zero,
                When::Always,
            ),
            g(
                "write-tier-digest-mismatches",
                count(digest),
                zero,
                When::Always,
            ),
            g(
                "restarts",
                count(restarts.map(|n| n as f64)),
                zero,
                When::Always,
            ),
        ];
        let load = self.load();
        let end = self.decide(&gates, &load, o.at);
        Decision {
            gates: gates.into_iter().map(|(g, _)| g).collect(),
            load,
            end,
        }
    }

    /// The under-replicated stretch: above zero outside a node step starts or keeps it; zero,
    /// or a node step, ends it; an absent series changes nothing (it fails at the end).
    fn follow_under_replicated(&mut self, under: Sample, o: &Observation<'_>) {
        match under {
            Sample::Value(v) if v > 0.0 => {
                if o.updater.is_some_and(|l| l.in_step(o.at)) {
                    self.state.under_since = None;
                } else {
                    self.state.under_since.get_or_insert(o.at);
                }
            }
            Sample::Value(_) => self.state.under_since = None,
            Sample::Absent => {}
        }
    }

    /// The error-rate sample and its limit. Without a baseline, or without the counters,
    /// the sample is absent (red) and the limit a placeholder.
    fn error_gate(&self, ratio: Option<f64>) -> (Sample, Limit) {
        match (self.state.baseline.error_ratio, ratio) {
            (Some(base), Some(r)) => (
                Sample::Value(r),
                Limit::AtMost((base * ERROR_BASELINE_FACTOR).max(base + ERROR_ADD_POINTS / 100.0)),
            ),
            _ => (Sample::Absent, Limit::AtMost(0.0)),
        }
    }

    /// The queue-to-start p99 of the bake as a multiple of the baseline's; `None` without
    /// either (a baseline of zero is none).
    fn latency_ratio(&self) -> Option<f64> {
        let base = self.state.baseline.queue_p99.filter(|b| *b > 0.0)?;
        Some(percentile(&self.state.counters.queued, 0.99)? / base)
    }

    fn load(&self) -> MinLoad {
        let actions = self.state.counters.requests.map_or(0.0, |r| r.increase);
        let floor = |measured: u64, required: u64| Floor { measured, required };
        MinLoad {
            actions: floor(actions as u64, MIN_ACTIONS),
            builds: floor(self.state.builds, MIN_BUILDS),
            hours: floor(self.state.load_hours.len() as u64, MIN_HOURS),
        }
    }

    fn decide(&self, gates: &[(GateResult, When)], load: &MinLoad, at: u64) -> Option<End> {
        let last = self.watched(at) >= self.state.bake_secs;
        let floor = load.actions.met();
        let red = gates.iter().find(|(g, when)| {
            g.verdict == Verdict::Red
                && (last || *when == When::Always || (*when == When::AfterFloor && floor))
        });
        if let Some((g, _)) = red {
            let measured = g
                .measured
                .map_or("no finite value".to_owned(), |v| v.to_string());
            return Some(End {
                status: Status::Red,
                note: format!(
                    "gate `{}` is red: measured {measured}, limit {:?}",
                    g.name, g.threshold
                ),
            });
        }
        if !last {
            return None;
        }
        if load.met() || self.state.min_load_waived() {
            return Some(End {
                status: Status::Green,
                note: String::new(),
            });
        }
        Some(End {
            status: Status::Extended,
            note: format!(
                "minimum load not met: actions {}/{}, builds {}/{}, hours {}/{}",
                load.actions.measured,
                load.actions.required,
                load.builds.measured,
                load.builds.required,
                load.hours.measured,
                load.hours.required
            ),
        })
    }
}

/// Errors over requests across the bake; no requests is no errors (the minimum load, not
/// this gate, holds a bake with no usage).
fn error_ratio(errors: Option<f64>, requests: Option<f64>) -> Option<f64> {
    let (e, r) = (errors?, requests?);
    Some(if r > 0.0 { e / r } else { 0.0 })
}

/// Follow every bucket of the queue-to-start histogram; whether this scrape had it.
fn follow_histogram(acc: &mut Vec<BucketCount>, m: &Metrics) -> bool {
    let Some(buckets) = m.buckets(metric::QUEUED_SECONDS) else {
        return false;
    };
    for (bound, count) in buckets {
        let le = bound.is_finite().then_some(bound);
        let at = match acc.iter().position(|b| b.le == le) {
            Some(i) => i,
            None => {
                acc.push(BucketCount {
                    le,
                    count: Counter {
                        last: count,
                        increase: 0.0,
                    },
                });
                acc.len() - 1
            }
        };
        acc[at].count.advance(count);
    }
    true
}

/// The smallest bucket bound that holds `q` of the observations added since the bake began;
/// zero when none were added. `None` when the histogram has not been seen.
fn percentile(acc: &[BucketCount], q: f64) -> Option<f64> {
    if acc.is_empty() {
        return None;
    }
    let mut sorted: Vec<&BucketCount> = acc.iter().collect();
    sorted.sort_by(|a, b| {
        a.le.unwrap_or(f64::INFINITY)
            .total_cmp(&b.le.unwrap_or(f64::INFINITY))
    });
    let total = sorted.iter().map(|b| b.count.increase).fold(0.0, f64::max);
    if total <= 0.0 {
        return Some(0.0);
    }
    sorted
        .iter()
        .find(|b| b.count.increase >= q * total)
        .map(|b| b.le.unwrap_or(f64::INFINITY))
}

impl Evaluator {
    /// Record that the bake ended `status` (green or red), so a restart does not go on.
    pub fn mark_ended(&mut self, status: Status) {
        self.state.ended = Some(status);
    }
}
