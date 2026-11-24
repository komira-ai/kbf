//! A family of duration histograms, one series per value of its labels, for `GET /metrics`: the
//! phases of a run (`kbf_executor_phase_seconds`, [`crate::executor::phases`]) and the steps
//! of its preparation and submit, by the form of its action tree (`kbf_executor_step_seconds`,
//! [`crate::executor::steps`]).
//!
//! The buckets reach from a millisecond, where a warm preparation and a submit on a warm
//! connection fall, to an hour, where a queued job can.

use std::fmt::Write as _;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The upper bounds of the buckets, in microseconds, with the `le` label of each.
pub(crate) const BOUNDS: [(u64, &str); 17] = [
    (1_000, "0.001"),
    (2_500, "0.0025"),
    (5_000, "0.005"),
    (10_000, "0.01"),
    (25_000, "0.025"),
    (50_000, "0.05"),
    (100_000, "0.1"),
    (250_000, "0.25"),
    (500_000, "0.5"),
    (1_000_000, "1"),
    (2_500_000, "2.5"),
    (5_000_000, "5"),
    (10_000_000, "10"),
    (30_000_000, "30"),
    (120_000_000, "120"),
    (600_000_000, "600"),
    (3_600_000_000, "3600"),
];

/// The values of a family's labels: the series of the family, in the order it renders them.
pub(crate) trait Series: Copy + 'static {
    /// The family's name.
    const NAME: &'static str;
    /// The family's help text.
    const HELP: &'static str;
    /// Every value, in the order the family lists them.
    const ALL: &'static [Self];

    /// The series' labels as the text format writes them between the braces, `le` aside:
    /// `phase="queue"`, say.
    fn labels(self) -> String;

    /// The value's place in [`Series::ALL`].
    fn slot(self) -> usize;
}

/// The histograms of one family.
#[derive(Debug)]
pub(crate) struct Histograms<S> {
    /// Per series, the observations that fell in each bucket (the last is over every bound).
    buckets: Vec<[AtomicU64; BOUNDS.len() + 1]>,
    micros: Vec<AtomicU64>,
    series: PhantomData<S>,
}

impl<S: Series> Default for Histograms<S> {
    fn default() -> Self {
        Self {
            buckets: S::ALL.iter().map(|_| Default::default()).collect(),
            micros: S::ALL.iter().map(|_| AtomicU64::new(0)).collect(),
            series: PhantomData,
        }
    }
}

impl<S: Series> Histograms<S> {
    /// Counts one `series` of `took`.
    pub(crate) fn observe(&self, series: S, took: Duration) {
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        let bucket = BOUNDS
            .iter()
            .position(|(bound, _)| micros <= *bound)
            .unwrap_or(BOUNDS.len());
        // Every series has its slot: the vectors are made from `S::ALL`.
        let slot = series.slot();
        self.buckets[slot][bucket].fetch_add(1, Ordering::Relaxed);
        self.micros[slot].fetch_add(micros, Ordering::Relaxed);
    }

    /// Appends the family to `out`, in the Prometheus text format.
    pub(crate) fn render(&self, out: &mut String) {
        let name = S::NAME;
        let _ = writeln!(out, "# HELP {name} {}\n# TYPE {name} histogram", S::HELP);
        let rows = S::ALL.iter().zip(self.buckets.iter().zip(&self.micros));
        for (series, (buckets, micros)) in rows {
            let labels = series.labels();
            let mut seen = 0;
            let bounds = BOUNDS.iter().map(|(_, le)| *le).chain(["+Inf"]);
            for (le, count) in bounds.zip(buckets) {
                seen += count.load(Ordering::Relaxed);
                let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{le}\"}} {seen}");
            }
            let micros = micros.load(Ordering::Relaxed);
            let (whole, part) = (micros / 1_000_000, micros % 1_000_000);
            let _ = writeln!(out, "{name}_sum{{{labels}}} {whole}.{part:06}");
            let _ = writeln!(out, "{name}_count{{{labels}}} {seen}");
        }
    }
}
