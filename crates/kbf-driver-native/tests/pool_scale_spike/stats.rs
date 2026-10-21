//! Timing summaries and the `MEASURE` line format of the spike.

use std::time::Duration;

/// Durations of one repeated step.
#[derive(Debug, Default)]
pub struct Samples(Vec<Duration>);

/// What [`Samples::summary`] says of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub n: usize,
    pub min: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub total: Duration,
}

impl Samples {
    pub fn push(&mut self, d: Duration) {
        self.0.push(d);
    }

    pub fn summary(&self) -> Option<Summary> {
        let mut sorted = self.0.clone();
        sorted.sort_unstable();
        let (first, last) = (*sorted.first()?, *sorted.last()?);
        let total: Duration = sorted.iter().sum();
        let n = u32::try_from(sorted.len()).ok()?;
        Some(Summary {
            n: sorted.len(),
            min: first,
            p50: percentile(&sorted, 50),
            p95: percentile(&sorted, 95),
            max: last,
            mean: total / n,
            total,
        })
    }
}

/// The nearest-rank percentile `p` (1 to 100) of `sorted`, which is not empty.
pub fn percentile(sorted: &[Duration], p: usize) -> Duration {
    let rank = (p * sorted.len()).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Milliseconds with microsecond resolution: `12.345`.
pub fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1000.0)
}

impl Summary {
    /// `n=20 min_ms=.. p50_ms=.. p95_ms=.. max_ms=.. mean_ms=.. total_ms=..`
    pub fn fields(&self) -> String {
        format!(
            "n={} min_ms={} p50_ms={} p95_ms={} max_ms={} mean_ms={} total_ms={}",
            self.n,
            ms(self.min),
            ms(self.p50),
            ms(self.p95),
            ms(self.max),
            ms(self.mean),
            ms(self.total)
        )
    }
}

/// A `MEASURE <what> <fields>` line.
pub fn measure(what: &str, fields: &str) -> String {
    format!("MEASURE {what} {fields}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn nearest_rank_percentiles_pick_the_value_at_the_rank() {
        let mut s = Samples::default();
        for v in (1..=20).rev() {
            s.push(d(v));
        }
        let got = s.summary().expect("samples");
        assert_eq!((got.n, got.min, got.max), (20, d(1), d(20)));
        assert_eq!(got.p50, d(10));
        assert_eq!(got.p95, d(19));
        assert_eq!(got.total, d(210));
        assert_eq!(got.mean, Duration::from_micros(10_500));
    }

    #[test]
    fn one_sample_is_every_percentile_and_none_is_no_summary() {
        let mut s = Samples::default();
        assert_eq!(s.summary(), None);
        s.push(d(7));
        let got = s.summary().expect("one");
        assert_eq!(
            (got.p50, got.p95, got.max, got.mean),
            (d(7), d(7), d(7), d(7))
        );
    }

    #[test]
    fn a_measure_line_names_what_and_its_fields() {
        let mut s = Samples::default();
        s.push(d(2));
        assert_eq!(
            measure("x", &s.summary().expect("one").fields()),
            "MEASURE x n=1 min_ms=2.000 p50_ms=2.000 p95_ms=2.000 max_ms=2.000 mean_ms=2.000 total_ms=2.000"
        );
    }
}
