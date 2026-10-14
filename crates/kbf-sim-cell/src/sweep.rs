//! The seed sweep: one scenario per seed, every seed run to its end even when an
//! earlier one breaks a property, and every failing seed reported with its replay line.

use std::any::Any;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{Kind, RunReport, Scenario, run};

/// A seed whose run broke a property (or panicked otherwise).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: Kind,
    pub seed: u64,
    /// The panic's message: for a broken property, the property and the replay line.
    pub message: String,
}

/// What a sweep did: a report per seed that finished, and every failure.
#[derive(Debug, Default)]
pub struct Sweep {
    pub reports: Vec<(u64, RunReport)>,
    pub failures: Vec<Failure>,
}

impl Sweep {
    /// The reports, for assertions that every seed (not one) exercised its faults.
    pub fn each(&self) -> impl Iterator<Item = &RunReport> {
        self.reports.iter().map(|(_, r)| r)
    }

    /// Panics listing every failing seed, if any failed.
    ///
    /// # Panics
    ///
    /// When a seed failed: the message counts the failures and gives each one's
    /// kind, seed and message.
    #[track_caller]
    pub fn assert_clean(&self) {
        if self.failures.is_empty() {
            return;
        }
        let lines: Vec<String> = self
            .failures
            .iter()
            .map(|f| format!("{:?} seed {}: {}", f.kind, f.seed, f.message))
            .collect();
        panic!(
            "{} of {} seeds failed:\n{}",
            self.failures.len(),
            self.failures.len() + self.reports.len(),
            lines.join("\n")
        );
    }
}

/// Runs the scenario `scenario` gives for each seed of `seeds`, in order.
pub fn sweep(seeds: Range<u64>, scenario: impl Fn(u64) -> Scenario) -> Sweep {
    let mut out = Sweep::default();
    for seed in seeds {
        let s = scenario(seed);
        match catch_unwind(AssertUnwindSafe(|| run(&s))) {
            Ok(report) => out.reports.push((seed, report)),
            Err(payload) => {
                out.failures.push(Failure {
                    kind: s.kind,
                    seed: s.seed,
                    message: message(payload.as_ref()),
                });
            }
        }
    }
    out
}

/// A panic's message: `panic!` with arguments carries a `String`, without a `&str`.
fn message(payload: &(dyn Any + Send)) -> String {
    let text = payload.downcast_ref::<&str>().copied();
    payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| text.unwrap_or("a panic without a message").to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a sweep that stops at the first failing seed, drops a failure, or
    /// loses the seed or message of one.
    #[test]
    fn every_seed_runs_and_every_failure_is_kept() {
        let swept = sweep(0..3, |seed| {
            let mut s = Scenario::new(Kind::Calm, seed);
            if seed != 1 {
                // An action no client is ever answered: property 7 breaks at the end.
                s.end = kbf_types::FarmTime::from_millis(1);
            }
            s
        });
        assert_eq!(swept.reports.len(), 1);
        assert_eq!(swept.reports[0].0, 1);
        let seeds: Vec<u64> = swept.failures.iter().map(|f| f.seed).collect();
        assert_eq!(seeds, [0, 2]);
        assert!(
            swept.failures[0].message.contains("property 7 (liveness)"),
            "{:?}",
            swept.failures[0]
        );
        let all = catch_unwind(AssertUnwindSafe(|| swept.assert_clean()));
        let message = all.expect_err("two failed");
        let message = message
            .downcast_ref::<String>()
            .expect("a formatted message");
        assert!(message.starts_with("2 of 3 seeds failed:\nCalm seed 0: "));
        assert!(message.contains("\nCalm seed 2: "));
        sweep(1..2, |seed| Scenario::new(Kind::Calm, seed)).assert_clean();
    }

    /// Catches a failure whose message is lost because its panic carried a `&str`, or
    /// carried no text at all.
    #[test]
    fn a_panic_message_is_kept_whatever_its_type() {
        assert_eq!(message(&String::from("formatted")), "formatted");
        assert_eq!(message(&"literal"), "literal");
        assert_eq!(message(&7_u8), "a panic without a message");
    }
}
