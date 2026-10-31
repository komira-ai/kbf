//! Percentiles and their bootstrap interval, nearest rank.
//!
//! The rule of the tool: a p99 is claimed only from the pooled samples of quiet passes, and only
//! with [`P99_MIN_N`] of them or more; below that the table shows p50, p95 and the maximum.

use crate::cells::content::Rng;

/// Fewest pooled quiet samples a p99 may be claimed from.
pub const P99_MIN_N: usize = 1000;

/// Resamples of the bootstrap.
pub const RESAMPLES: u32 = 300;

/// The `pct`-th percentile of `sorted` (ascending), nearest rank: the value at rank
/// `ceil(pct / 100 * n)`. `None` for an empty slice.
#[must_use]
pub fn nearest_rank(sorted: &[u64], pct: u32) -> Option<u64> {
    let n = sorted.len() as u64;
    let rank = (n * u64::from(pct)).div_ceil(100).max(1);
    sorted
        .get(usize::try_from(rank).unwrap_or(usize::MAX) - 1)
        .copied()
}

/// The summary of a set of latencies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Median.
    pub p50: u64,
    /// 95th percentile.
    pub p95: u64,
    /// 99th percentile, always computed here; whether it may be claimed is [`p99_claimable`].
    pub p99: u64,
    /// Largest.
    pub max: u64,
}

/// Sorts `values` and summarises them; `None` if there are none.
#[must_use]
pub fn summarise(values: &mut [u64]) -> Option<Summary> {
    values.sort_unstable();
    let max = *values.last()?;
    let at = |pct| nearest_rank(values, pct).unwrap_or(max);
    Some(Summary {
        p50: at(50),
        p95: at(95),
        p99: at(99),
        max,
    })
}

/// Whether a p99 may be claimed from `quiet_n` pooled samples of quiet passes.
#[must_use]
pub fn p99_claimable(quiet_n: usize) -> bool {
    quiet_n >= P99_MIN_N
}

/// A 95% interval of the `pct`-th percentile of `values` by bootstrap: the 2.5th and 97.5th
/// percentile of `RESAMPLES` resampled percentiles (by position in the sorted resamples). The resampling is seeded, so a report of the
/// same files prints the same interval. `None` for no values.
#[must_use]
pub fn bootstrap_ci(values: &[u64], pct: u32, seed: u64) -> Option<(u64, u64)> {
    let n = values.len() as u64;
    if n == 0 {
        return None;
    }
    let mut rng = Rng::new(seed);
    let mut sample = vec![0u64; values.len()];
    let mut stats: Vec<u64> = (0..RESAMPLES)
        .map(|_| {
            for slot in &mut sample {
                *slot = values[usize::try_from(rng.below(n)).expect("an index")];
            }
            sample.sort_unstable();
            nearest_rank(&sample, pct).expect("a sample is not empty")
        })
        .collect();
    stats.sort_unstable();
    let low = (RESAMPLES * 25).div_ceil(1000) as usize - 1;
    let high = (RESAMPLES * 975).div_ceil(1000) as usize - 1;
    Some((stats[low], stats[high]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches an off-by-one of the nearest-rank rule: with 1..=100 the p50 is 50, the p99 99,
    /// and with 10 values the p95 is the tenth (ceil of 9.5), not the ninth.
    #[test]
    fn nearest_rank_takes_the_ceiling_rank() {
        let hundred: Vec<u64> = (1..=100).collect();
        assert_eq!(nearest_rank(&hundred, 50), Some(50));
        assert_eq!(nearest_rank(&hundred, 99), Some(99));
        assert_eq!(nearest_rank(&hundred, 100), Some(100));
        let ten: Vec<u64> = (1..=10).collect();
        assert_eq!(nearest_rank(&ten, 95), Some(10));
        assert_eq!(nearest_rank(&ten, 50), Some(5));
        assert_eq!(nearest_rank(&[42], 99), Some(42));
        assert_eq!(nearest_rank(&[], 50), None);
    }

    /// Catches a summary that does not sort its input, or whose maximum is not the largest.
    #[test]
    fn a_summary_sorts_and_names_the_extremes() {
        let mut values = vec![9, 1, 5, 3, 7, 2, 8, 4, 6, 10];
        let s = summarise(&mut values).unwrap();
        assert_eq!(
            s,
            Summary {
                p50: 5,
                p95: 10,
                p99: 10,
                max: 10
            }
        );
        assert_eq!(summarise(&mut []), None);
    }

    /// Catches the 1,000-sample rule drifting: 999 may not claim a p99, 1,000 may.
    #[test]
    fn a_p99_needs_a_thousand_quiet_samples() {
        assert!(!p99_claimable(999));
        assert!(p99_claimable(1000));
        assert!(!p99_claimable(0));
    }

    /// Catches an interval that does not bracket the percentile it is for, one that depends on
    /// anything but its input and seed, and the empty input.
    #[test]
    fn the_bootstrap_interval_brackets_the_median_and_repeats() {
        let values: Vec<u64> = (1..=500).collect();
        let (low, high) = bootstrap_ci(&values, 50, 1).unwrap();
        assert!(low <= 250, "{low}..{high}");
        assert!(250 <= high, "{low}..{high}");
        assert!(low < high);
        assert_eq!(bootstrap_ci(&values, 50, 1), Some((low, high)));
        assert_eq!(bootstrap_ci(&[], 50, 1), None);
        assert_eq!(bootstrap_ci(&[7], 99, 1), Some((7, 7)));
    }
}
