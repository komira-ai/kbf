//! Per-crate line and branch coverage for the workspace, and the gate that keeps it up.
//!
//! CI runs the fast tests (the nextest `pr` profile) under `cargo llvm-cov --branch`,
//! which writes an lcov tracefile. This crate reads it ([`lcov`]), sums its per-file
//! counts into one [`CrateCoverage`] per directory under `crates/`
//! ([`gate::by_crate`]) and fails any crate whose line or branch coverage is under its
//! threshold ([`gate`]). The threshold is 99.00% ([`Percent::TARGET`]) unless the
//! `coverage-floors` file ([`floors`]) lists the crate with a lower floor for that
//! metric. A floor is the crate's current coverage, floored to hundredths of a
//! percent; it may only rise: `kbf-coverage raise` ([`raise`]) lifts floors to what a
//! run measured and never adds or lowers one, and `kbf-coverage no-lowering` fails a
//! pull request whose floors file is lower anywhere than its base's. When CI splits the
//! tests over shards, `kbf-coverage merge` ([`merge`]) first adds the shards' tracefiles
//! into the one a single run would have written, and the gate reads that.
//!
//! Percentages are floored to hundredths ([`Percent`]), so one uncovered line in a
//! large crate shows as 99.99%, never as a rounded-up 100.00%. The gate compares in
//! integers (`hit * 10000 >= percent * found`, see [`Counts::meets`]), never floats.

pub mod floors;
pub mod gate;
pub mod lcov;
pub mod merge;
pub mod raise;
pub mod renames;

use std::fmt;

/// How many items (lines or branches) a source holds and how many of them ran.
/// `hit <= found` always holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    found: u64,
    hit: u64,
}

impl Counts {
    /// `None` when `hit > found`, which no coverage tool can report.
    pub fn new(found: u64, hit: u64) -> Option<Self> {
        (hit <= found).then_some(Self { found, hit })
    }

    pub fn found(self) -> u64 {
        self.found
    }

    pub fn hit(self) -> u64 {
        self.hit
    }

    /// How many items did not run, or `None` when there is nothing to measure.
    pub fn missed(self) -> Option<u64> {
        (self.found > 0).then(|| self.found - self.hit)
    }

    /// The sum of two counts; saturates rather than wrapping.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            found: self.found.saturating_add(other.found),
            hit: self.hit.saturating_add(other.hit),
        }
    }

    /// Whether the share that ran is at least `p`; true when there is nothing to
    /// measure. Exact: no division, so `99.00` of 10000 items passes at 9900 hits and
    /// fails at 9899.
    pub fn meets(self, p: Percent) -> bool {
        u128::from(self.hit) * 10_000 >= u128::from(p.0) * u128::from(self.found)
    }

    /// The share that ran, or `None` when there is nothing to measure.
    pub fn percent(self) -> Option<Percent> {
        if self.found == 0 {
            return None;
        }
        let hundredths = u128::from(self.hit) * 10_000 / u128::from(self.found);
        // `hit <= found`, so `hundredths <= 10_000` and the cast cannot truncate.
        Some(Percent(hundredths as u16))
    }
}

/// Line and branch counts for one crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrateCoverage {
    pub lines: Counts,
    pub branches: Counts,
}

impl CrateCoverage {
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            lines: self.lines.plus(other.lines),
            branches: self.branches.plus(other.branches),
        }
    }
}

/// A percentage in hundredths, from 0.00 to 100.00, floored (never rounded up).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Percent(u16);

impl Percent {
    pub const FULL: Percent = Percent(10_000);
    /// What every crate must reach unless its floor says otherwise: 99.00%.
    pub const TARGET: Percent = Percent(9_900);

    /// `None` above 100.00%.
    pub fn from_hundredths(h: u16) -> Option<Self> {
        (h <= Self::FULL.0).then_some(Self(h))
    }

    /// Reads what [`Display`](fmt::Display) writes: digits, a point, two digits, at
    /// most `100.00`. `99`, `99.5`, `+99.00` and `1e2` are refused.
    pub fn parse(s: &str) -> Option<Self> {
        let (whole, frac) = s.split_once('.')?;
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        if !digits(whole) || frac.len() != 2 || !digits(frac) || whole.len() > 3 {
            return None;
        }
        let h = whole.parse::<u16>().ok()? * 100 + frac.parse::<u16>().ok()?;
        Self::from_hundredths(h)
    }

    /// What is left to 100%.
    #[must_use]
    pub fn gap(self) -> Percent {
        Percent(Self::FULL.0 - self.0)
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}", self.0 / 100, self.0 % 100)
    }
}

/// Formats an optional value as the baseline file and the table write it: `-` when
/// there is nothing to measure.
pub fn show<T: fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(found: u64, hit: u64) -> Counts {
        Counts::new(found, hit).expect("valid")
    }

    fn pct(c: Counts) -> String {
        show(c.percent())
    }

    /// Catches: a percentage rounded up, which would show one uncovered line in a large
    /// crate as 100.00% with no gap.
    #[test]
    fn percent_floors_so_a_miss_is_never_hidden() {
        let c = counts(100_000, 99_999);
        assert_eq!(pct(c), "99.99");
        assert_eq!(show(c.percent().map(Percent::gap)), "0.01");
        assert_eq!(pct(counts(3, 2)), "66.66");
        assert_eq!(pct(counts(20, 1)), "5.00");
        assert_eq!(counts(4, 4).percent(), Some(Percent::FULL));
        assert_eq!(pct(counts(4, 0)), "0.00");
    }

    /// Catches: a crate with nothing to measure reported as 0% or 0 missed (values the
    /// baseline could not tell from a real measurement).
    #[test]
    fn nothing_found_has_no_percentage_and_no_miss_count() {
        assert_eq!(Counts::default().percent(), None);
        assert_eq!(Counts::default().missed(), None);
        assert_eq!(show::<Percent>(None), "-");
        assert_eq!(show(Some(7)), "7");
    }

    /// Catches: the missed count computed from the wrong field, which would let a drop
    /// in hits pass the gate.
    #[test]
    fn missed_is_found_minus_hit() {
        assert_eq!(counts(10, 7).missed(), Some(3));
        assert_eq!(counts(4, 4).missed(), Some(0));
        assert_eq!(counts(4, 0).missed(), Some(4));
    }

    /// Catches: counts that claim more hits than items, which would yield over 100%.
    #[test]
    fn hits_above_found_are_refused() {
        assert_eq!(Counts::new(1, 2), None);
        let c = counts(2, 1);
        assert_eq!((c.found(), c.hit()), (2, 1));
    }

    /// Catches: a sum that adds the wrong fields or wraps on overflow.
    #[test]
    fn counts_add_field_by_field_and_saturate() {
        let a = CrateCoverage {
            lines: counts(10, 4),
            branches: counts(2, 1),
        };
        let b = CrateCoverage {
            lines: counts(5, 5),
            branches: counts(3, 0),
        };
        let sum = a.plus(b);
        assert_eq!(sum.lines, counts(15, 9));
        assert_eq!(sum.branches, counts(5, 1));
        let max = counts(u64::MAX, u64::MAX);
        assert_eq!(max.plus(max), max);
        assert_eq!(max.percent(), Some(Percent::FULL));
    }

    /// Catches: a gate that compares floats or rounds, or `>` for `>=`: exactly the
    /// threshold passes, one hit fewer fails, and an empty source always passes.
    #[test]
    fn meets_is_exact_at_the_threshold() {
        let t = Percent::TARGET;
        assert!(counts(10_000, 9_900).meets(t));
        assert!(!counts(10_000, 9_899).meets(t));
        assert!(counts(100, 99).meets(t));
        assert!(!counts(101, 99).meets(t), "98.01% is under 99.00%");
        assert!(counts(200, 198).meets(t));
        assert!(Counts::default().meets(Percent::FULL));
        assert!(counts(u64::MAX, u64::MAX).meets(Percent::FULL));
        assert!(!counts(u64::MAX, u64::MAX - 1).meets(Percent::FULL));
        // Whatever `percent` reports is met by the counts it came from.
        for (f, h) in [(3, 2), (7, 5), (9_103, 9_031), (1_000_003, 999_999)] {
            let c = counts(f, h);
            assert!(c.meets(c.percent().expect("found")), "{h}/{f}");
        }
    }

    /// Catches: a floor read back as another number, or a malformed one accepted.
    #[test]
    fn percent_text_round_trips_and_refuses_the_rest() {
        for text in ["0.00", "5.05", "97.43", "99.00", "100.00"] {
            let p = Percent::parse(text).expect(text);
            assert_eq!(p.to_string(), text);
        }
        for bad in [
            "", "99", "99.", ".99", "99.5", "99.000", "+99.00", "-1.00", "1e2", "100.01",
            "1000.00", "9a.00", "99.0a", "0x9.00",
        ] {
            assert_eq!(Percent::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Percent::from_hundredths(10_000), Some(Percent::FULL));
        assert_eq!(Percent::from_hundredths(10_001), None);
    }
}
