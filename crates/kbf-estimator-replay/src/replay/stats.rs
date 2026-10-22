//! What a replay measures, per window of days.

use std::collections::BTreeMap;

use kbf_estimator::Level;

use super::sim::{ArmRun, Lookup};

const DAY_MS: u64 = 86_400_000;
/// The normal quantile of a two-sided 95% interval.
const Z95: f64 = 1.96;

/// A count out of a total, with its interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rate {
    /// Events.
    pub hits: u64,
    /// Trials.
    pub of: u64,
}

impl Rate {
    /// Events per thousand trials; 0 for no trials.
    pub fn per_1000(self) -> f64 {
        if self.of == 0 {
            0.0
        } else {
            1000.0 * self.hits as f64 / self.of as f64
        }
    }

    /// The Wilson 95% interval, per thousand trials; `(0, 0)` for no trials.
    pub fn wilson_per_1000(self) -> (f64, f64) {
        if self.of == 0 {
            return (0.0, 0.0);
        }
        let n = self.of as f64;
        let p = self.hits as f64 / n;
        let z2 = Z95 * Z95;
        let centre = (p + z2 / (2.0 * n)) / (1.0 + z2 / n);
        let half = Z95 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / (1.0 + z2 / n);
        (
            1000.0 * (centre - half).max(0.0),
            1000.0 * (centre + half).min(1.0),
        )
    }
}

/// The value at percentile `p` (1 to 100) of `sorted`, by nearest rank.
pub fn percentile(sorted: &[u64], p: u64) -> Option<u64> {
    let n = sorted.len() as u64;
    let rank = (p * n).div_ceil(100).max(1);
    sorted.get(usize::try_from(rank - 1).ok()?).copied()
}

/// Days of the trace, counted from 1, both ends included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// First day.
    pub first: u32,
    /// Last day.
    pub last: u32,
}

impl Window {
    fn holds(self, arrives_ms: u64) -> bool {
        let day = 1 + arrives_ms / DAY_MS;
        (u64::from(self.first)..=u64::from(self.last)).contains(&day)
    }
}

/// Queue wait percentiles in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waits {
    /// Median.
    pub p50: u64,
    /// 95th percentile.
    pub p95: u64,
    /// 99th percentile.
    pub p99: u64,
}

/// What an arm did to the operations that arrived in a window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Section {
    /// Operations that ran their own attempts.
    pub ops: u64,
    /// Operations that joined one in flight.
    pub joined: u64,
    /// Operations the scheduler refused.
    pub refused: u64,
    /// Attempts, and how many were own-limit kills: per 1000 attempts.
    pub kills: Rate,
    /// Operations that were killed at least once (climbed the ladder), out of operations.
    pub climbed: Rate,
    /// Queue wait (queued to started) of every attempt; `None` for no attempts.
    pub waits: Option<Waits>,
    /// For each day of the window that had arrivals, the time after its end until every operation that had
    /// arrived by then was done, in milliseconds (0 for no backlog).
    pub drain_ms: Vec<(u64, u64)>,
    /// Operations resolved by an estimate at L1.
    pub l1: u64,
    /// At L2.
    pub l2: u64,
    /// With no estimate, by reason (`NoKey` is the action without a shape).
    pub cold: BTreeMap<String, u64>,
    /// Operations whose key was mixed when they arrived.
    pub mixed: u64,
    /// Operations the arm booked above what the record's first attempt did.
    pub raised: u64,
}

/// The measures of the operations of `run` that arrived in `w`.
pub fn section(run: &ArmRun, w: Window) -> Section {
    let mut s = Section::default();
    let mut in_window = Vec::new();
    for (i, op) in run.ops.iter().enumerate() {
        if !w.holds(op.arrives_ms) {
            continue;
        }
        if op.joined {
            s.joined += 1;
            continue;
        }
        in_window.push(i);
        s.ops += 1;
        s.refused += u64::from(op.refused);
        s.mixed += u64::from(op.mixed);
        s.raised += u64::from(op.raised);
        match op.lookup {
            Lookup::Level(Level::L1) => s.l1 += 1,
            Lookup::Level(Level::L2) => s.l2 += 1,
            Lookup::Cold(why) => *s.cold.entry(format!("{why:?}")).or_default() += 1,
            Lookup::NoKey => *s.cold.entry("NoKey".to_owned()).or_default() += 1,
        }
    }
    let mut waits = Vec::new();
    let mut killed_ops = std::collections::BTreeSet::new();
    for a in &run.attempts {
        if in_window.binary_search(&a.op).is_err() {
            continue;
        }
        s.kills.of += 1;
        s.kills.hits += u64::from(a.killed);
        if a.killed {
            killed_ops.insert(a.op);
        }
        waits.push(a.started_ms - a.queued_ms);
    }
    s.climbed = Rate {
        hits: killed_ops.len() as u64,
        of: s.ops,
    };
    waits.sort_unstable();
    s.waits = percentile(&waits, 50)
        .zip(percentile(&waits, 95))
        .zip(percentile(&waits, 99))
        .map(|((p50, p95), p99)| Waits { p50, p95, p99 });
    // Only days that had arrivals: the work is bounded by the trace, not by the numbers
    // a window or a time may hold.
    let days: std::collections::BTreeSet<u64> = in_window
        .iter()
        .map(|&i| 1 + run.ops[i].arrives_ms / DAY_MS)
        .collect();
    for day in days {
        let end = day.saturating_mul(DAY_MS);
        let last_done = run
            .ops
            .iter()
            .filter(|o| o.arrives_ms < end)
            .filter_map(|o| o.finished_ms)
            .max()
            .unwrap_or(0);
        s.drain_ms.push((day, last_done.saturating_sub(end)));
    }
    s
}

/// The null arm against the record: how many own-limit kills each shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fidelity {
    /// Kills in the record, over the operations that ran their own attempts.
    pub recorded: u64,
    /// Kills the replay made.
    pub simulated: u64,
}

impl Fidelity {
    /// Whether the two counts differ by at most `percent` of the recorded count.
    pub fn within_percent(self, percent: u64) -> bool {
        self.recorded.abs_diff(self.simulated) * 100 <= self.recorded * percent
    }
}

/// Compares the kills of `run` (the null arm) with those of `trace`.
pub fn fidelity(trace: &super::trace::Trace, run: &ArmRun) -> Fidelity {
    let recorded = trace
        .ops
        .iter()
        .zip(&run.ops)
        .filter(|(_, rec)| !rec.joined)
        .map(|(op, _)| u64::from(op.recorded_kills))
        .sum();
    let simulated = run.attempts.iter().filter(|a| a.killed).count() as u64;
    Fidelity {
        recorded,
        simulated,
    }
}
