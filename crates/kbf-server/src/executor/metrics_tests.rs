use kbf_proto::reapi::{ActionResult, Digest};
use kbf_proto::worker::Lost;
use kbf_types::LeaseId;

use super::*;
use crate::executor::outcome::{Settled, ran, result_of};

fn end_of(settled: &Settled) -> RunEnd {
    let action = Digest::default();
    RunEnd::of(&result_of(LeaseId::new(1, 1), &action, settled))
}

/// Catches a run counted under an ending the farm was not told: each way a result is formed
/// has its own label.
#[test]
fn each_result_is_counted_under_the_ending_the_farm_is_told() {
    let action = Digest::default();
    let lease = LeaseId::new(1, 1);
    assert_eq!(
        RunEnd::of(&ran(lease, &action, ActionResult::default())),
        RunEnd::Ran
    );
    assert_eq!(end_of(&Settled::Infra("x".to_owned())), RunEnd::Infra);
    assert_eq!(end_of(&Settled::Timeout), RunEnd::Timeout);
    assert_eq!(end_of(&Settled::OutOfMemory), RunEnd::OutOfMemory);
    assert_eq!(end_of(&Settled::Lost(Lost::Fenced)), RunEnd::Lost);
    assert_eq!(end_of(&Settled::Invalid("x".to_owned())), RunEnd::Invalid);
}

/// Catches a counter that lands in another's row: every ending and every ingest result has a
/// row of its own, in the order of the labels.
#[test]
fn every_ending_and_every_ingest_result_has_a_row_of_its_own() {
    let metrics = Metrics::default();
    for (n, end) in RunEnd::ALL.iter().enumerate() {
        for _ in 0..=n {
            metrics.run_ended(*end);
        }
    }
    let ingests: Vec<IngestEnd> = [IngestEnd::Ok, IngestEnd::Unavailable, IngestEnd::Stopped]
        .into_iter()
        .chain(
            REASONS
                .iter()
                .map(|(reason, _)| IngestEnd::Rejected(*reason)),
        )
        .collect();
    assert_eq!(ingests.len(), IngestEnd::labels().count());
    for (n, end) in ingests.iter().enumerate() {
        for _ in 0..=n {
            metrics.ingested(*end);
        }
    }
    let mut text = String::new();
    metrics.render(&mut text);
    for (n, end) in RunEnd::ALL.iter().enumerate() {
        let row = format!(
            "kbf_executor_runs_total{{outcome=\"{}\"}} {}\n",
            end.label(),
            n + 1
        );
        assert!(text.contains(&row), "{row}in {text}");
    }
    for (n, label) in IngestEnd::labels().enumerate() {
        let row = format!(
            "kbf_executor_ingests_total{{result=\"{label}\"}} {}\n",
            n + 1
        );
        assert!(text.contains(&row), "{row}in {text}");
    }
}

/// Catches the plain counters and the gauge: a host held is read as last set, and a late start
/// and a released stray are counted.
#[test]
fn the_gauge_is_what_was_last_held_and_the_counters_count() {
    let metrics = Metrics::default();
    metrics.hold(3);
    metrics.hold(2);
    metrics.late_start();
    metrics.late_start();
    metrics.stray_released();
    assert_eq!(metrics.held(), 2);
    let mut text = String::new();
    metrics.render(&mut text);
    for row in [
        "kbf_executor_leases_held 2\n",
        "kbf_executor_late_starts_total 2\n",
        "kbf_executor_stray_leases_released_total 1\n",
    ] {
        assert!(text.contains(row), "{row}in {text}");
    }
    for family in [
        "# TYPE kbf_executor_leases_held gauge",
        "# TYPE kbf_executor_runs_total counter",
        "# TYPE kbf_executor_ingests_total counter",
    ] {
        assert!(text.contains(family), "{family}");
    }
}
